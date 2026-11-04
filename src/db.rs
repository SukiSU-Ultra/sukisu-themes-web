use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Context;
use chrono::{Duration as ChronoDuration, Utc};
use rusqlite::{params, Connection, OpenFlags};
use tokio_rusqlite::Connection as AsyncConnection;

/// Per-connection SQLite tuning. WAL allows concurrent readers while a writer
/// holds the write lock; NORMAL avoids an fsync per transaction.
const CONNECTION_PRAGMAS: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=5000;
PRAGMA synchronous=NORMAL;
PRAGMA temp_store=MEMORY;
PRAGMA cache_size=-8000;
";

fn db_pool_size() -> usize {
    std::env::var("THEME_DB_POOL_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(1, 32)
}

/// A recent download is worth more than an old one when ranking by heat.
pub const HEAT_RECENT_WEIGHT: i64 = 10;
/// Heat counts package downloads from this many days, including today.
pub const HEAT_WINDOW_DAYS: i64 = 7;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    display_name TEXT NOT NULL,
    email TEXT UNIQUE COLLATE NOCASE,
    password_hash TEXT,
    avatar TEXT,
    role TEXT NOT NULL DEFAULT 'user',
    is_super_admin INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'email_pending',
    email_verified INTEGER NOT NULL DEFAULT 0,
    rejection_reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    approved_at TEXT,
    approved_by TEXT,
    last_login_at TEXT
);

CREATE TABLE IF NOT EXISTS user_identities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    provider_user_id TEXT NOT NULL,
    provider_login TEXT,
    email TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(provider, provider_user_id)
);
CREATE INDEX IF NOT EXISTS user_identities_user_idx ON user_identities(user_id);

CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_seen_at TEXT,
    user_agent TEXT,
    ip TEXT
);
CREATE INDEX IF NOT EXISTS sessions_user_idx ON sessions(user_id);
CREATE INDEX IF NOT EXISTS sessions_expiry_idx ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS email_tokens (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT,
    email TEXT NOT NULL,
    purpose TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed_at TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS email_tokens_email_idx ON email_tokens(email, purpose);

CREATE TABLE IF NOT EXISTS oauth_states (
    state TEXT PRIMARY KEY,
    user_id TEXT,
    link_user_id TEXT,
    redirect TEXT,
    expires_at INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS authors (
    hash TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    display_name TEXT,
    user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    auto_publish INTEGER NOT NULL DEFAULT 1,
    code TEXT UNIQUE,
    next_theme_seq INTEGER NOT NULL DEFAULT 1,
    approved_at TEXT,
    approved_by TEXT,
    last_used_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS authors_user_idx ON authors(user_id);
CREATE INDEX IF NOT EXISTS authors_status_idx ON authors(status);

CREATE TABLE IF NOT EXISTS author_requests (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    author_hash TEXT,
    display_name TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    note TEXT,
    created_at TEXT NOT NULL,
    reviewed_at TEXT,
    reviewed_by TEXT,
    reject_reason TEXT,
    generated_author_hash TEXT
);
CREATE INDEX IF NOT EXISTS author_requests_status_idx ON author_requests(status, created_at);
CREATE INDEX IF NOT EXISTS author_requests_user_idx ON author_requests(user_id);

CREATE TABLE IF NOT EXISTS author_reset_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,
    old_hash TEXT NOT NULL,
    new_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    created_by TEXT
);
CREATE INDEX IF NOT EXISTS author_reset_user_idx ON author_reset_history(user_id);

CREATE TABLE IF NOT EXISTS submissions (
    id TEXT PRIMARY KEY,
    user_id TEXT,
    theme_id TEXT NOT NULL,
    name TEXT,
    version INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL,
    submitted_at TEXT NOT NULL,
    reviewed_at TEXT,
    reviewed_by TEXT,
    reject_reason TEXT,
    auto_published INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS submissions_user_idx ON submissions(user_id, submitted_at);
CREATE INDEX IF NOT EXISTS submissions_status_idx ON submissions(status, submitted_at);
CREATE INDEX IF NOT EXISTS submissions_theme_idx ON submissions(theme_id);

CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id TEXT,
    action TEXT NOT NULL,
    target_type TEXT,
    target_id TEXT,
    detail TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_log_created_idx ON audit_log(created_at);

CREATE TABLE IF NOT EXISTS app_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS theme_stats (
    theme_id TEXT PRIMARY KEY,
    downloads INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS theme_download_days (
    theme_id TEXT NOT NULL,
    day TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (theme_id, day)
);
CREATE INDEX IF NOT EXISTS theme_download_days_day_idx ON theme_download_days(day);

CREATE TABLE IF NOT EXISTS theme_comments (
    id TEXT PRIMARY KEY,
    theme_id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    parent_id TEXT REFERENCES theme_comments(id) ON DELETE CASCADE,
    visibility TEXT NOT NULL DEFAULT 'public',
    content TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS theme_comments_theme_idx ON theme_comments(theme_id, created_at);
CREATE INDEX IF NOT EXISTS theme_comments_user_idx ON theme_comments(user_id, created_at);
CREATE INDEX IF NOT EXISTS theme_comments_parent_idx ON theme_comments(parent_id);
CREATE INDEX IF NOT EXISTS theme_comments_created_idx ON theme_comments(created_at);

CREATE TABLE IF NOT EXISTS theme_comment_mentions (
    comment_id TEXT NOT NULL REFERENCES theme_comments(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    PRIMARY KEY (comment_id, user_id)
);
CREATE INDEX IF NOT EXISTS theme_comment_mentions_user_idx
    ON theme_comment_mentions(user_id);
"#;

#[derive(Clone)]
pub struct Database {
    conns: Arc<Vec<AsyncConnection>>,
    next: Arc<AtomicUsize>,
}

impl Database {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("无法创建数据库目录 {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        // Each connection owns its worker thread in tokio-rusqlite, so a small
        // pool lets reads proceed while another request writes.
        let pool_size = db_pool_size();
        let mut conns = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let conn = AsyncConnection::open(path)
                .await
                .with_context(|| format!("无法打开数据库 {}", path.display()))?;
            conn.call(|conn| {
                conn.execute_batch(CONNECTION_PRAGMAS)?;
                Ok(())
            })
            .await
            .with_context(|| format!("无法配置数据库 {}", path.display()))?;
            conns.push(conn);
        }
        let db = Self {
            conns: Arc::new(conns),
            next: Arc::new(AtomicUsize::new(0)),
        };
        db.initialize().await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(db)
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        self.call(|conn| {
            conn.execute_batch(CONNECTION_PRAGMAS)?;
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version < 1 {
                conn.execute_batch(SCHEMA_V1)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            // Additive stats tables for databases created before download tracking.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS theme_stats (
                    theme_id TEXT PRIMARY KEY,
                    downloads INTEGER NOT NULL DEFAULT 0,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS theme_download_days (
                    theme_id TEXT NOT NULL,
                    day TEXT NOT NULL,
                    count INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (theme_id, day)
                );
                CREATE INDEX IF NOT EXISTS theme_download_days_day_idx
                    ON theme_download_days(day);
                CREATE TABLE IF NOT EXISTS theme_comments (
                    id TEXT PRIMARY KEY,
                    theme_id TEXT NOT NULL,
                    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    parent_id TEXT REFERENCES theme_comments(id) ON DELETE CASCADE,
                    visibility TEXT NOT NULL DEFAULT 'public',
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS theme_comments_theme_idx
                    ON theme_comments(theme_id, created_at);
                CREATE INDEX IF NOT EXISTS theme_comments_user_idx
                    ON theme_comments(user_id, created_at);
                CREATE INDEX IF NOT EXISTS theme_comments_parent_idx
                    ON theme_comments(parent_id);
                CREATE INDEX IF NOT EXISTS theme_comments_created_idx
                    ON theme_comments(created_at);
                CREATE TABLE IF NOT EXISTS theme_comment_mentions (
                    comment_id TEXT NOT NULL REFERENCES theme_comments(id) ON DELETE CASCADE,
                    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (comment_id, user_id)
                );
                CREATE INDEX IF NOT EXISTS theme_comment_mentions_user_idx
                    ON theme_comment_mentions(user_id);
                CREATE TABLE IF NOT EXISTS author_restrictions (
                    author_hash TEXT PRIMARY KEY,
                    features TEXT NOT NULL DEFAULT '',
                    updated_at TEXT NOT NULL,
                    updated_by TEXT
                );
                CREATE TABLE IF NOT EXISTS user_follows (
                    follower_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    author_hash TEXT NOT NULL REFERENCES authors(hash) ON DELETE CASCADE,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (follower_id, author_hash)
                );
                CREATE INDEX IF NOT EXISTS user_follows_author_idx
                    ON user_follows(author_hash);
                CREATE TABLE IF NOT EXISTS device_login_codes (
                    device_code_hash TEXT PRIMARY KEY,
                    user_code TEXT NOT NULL UNIQUE,
                    status TEXT NOT NULL DEFAULT 'pending',
                    user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
                    user_agent TEXT,
                    ip TEXT,
                    created_at TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    approved_at TEXT,
                    consumed_at TEXT
                );
                CREATE INDEX IF NOT EXISTS device_login_codes_user_code_idx
                    ON device_login_codes(user_code);
                CREATE INDEX IF NOT EXISTS device_login_codes_expiry_idx
                    ON device_login_codes(expires_at);
                CREATE TABLE IF NOT EXISTS user_preferences (
                    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
                    show_r18 INTEGER NOT NULL DEFAULT 0,
                    show_r18_set INTEGER NOT NULL DEFAULT 0,
                    consent_version INTEGER NOT NULL DEFAULT 0,
                    consent_at TEXT,
                    updated_at TEXT NOT NULL
                );",
            )?;
            // A consent-only sync also creates a preferences row. Keep explicit R18 choices
            // distinguishable so the manager can prefer the web setting after login without
            // treating a default 0 as an intentional "off".
            let has_show_r18_set: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('user_preferences') WHERE name = 'show_r18_set'",
                [],
                |row| row.get(0),
            )?;
            if has_show_r18_set == 0 {
                conn.execute(
                    "ALTER TABLE user_preferences ADD COLUMN show_r18_set INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
                conn.execute(
                    "UPDATE user_preferences SET show_r18_set = 1 WHERE show_r18 <> 0",
                    [],
                )?;
            }
            let has_super_admin: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name = 'is_super_admin'",
                [],
                |row| row.get(0),
            )?;
            if has_super_admin == 0 {
                conn.execute(
                    "ALTER TABLE users ADD COLUMN is_super_admin INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            let has_auto_publish: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('authors') WHERE name = 'auto_publish'",
                [],
                |row| row.get(0),
            )?;
            if has_auto_publish == 0 {
                conn.execute(
                    "ALTER TABLE authors ADD COLUMN auto_publish INTEGER NOT NULL DEFAULT 1",
                    [],
                )?;
            }
            let has_code: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('authors') WHERE name = 'code'",
                [],
                |row| row.get(0),
            )?;
            if has_code == 0 {
                conn.execute("ALTER TABLE authors ADD COLUMN code TEXT", [])?;
            }
            let has_seq: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('authors') WHERE name = 'next_theme_seq'",
                [],
                |row| row.get(0),
            )?;
            if has_seq == 0 {
                conn.execute(
                    "ALTER TABLE authors ADD COLUMN next_theme_seq INTEGER NOT NULL DEFAULT 1",
                    [],
                )?;
            }
            let has_auto_revoke: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('authors') WHERE name = 'auto_revoke_at'",
                [],
                |row| row.get(0),
            )?;
            if has_auto_revoke == 0 {
                conn.execute("ALTER TABLE authors ADD COLUMN auto_revoke_at TEXT", [])?;
            }
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS authors_code_idx ON authors(code) WHERE code IS NOT NULL",
                [],
            )?;
            // Account review was removed: only author applications and theme submissions
            // are reviewed now. Keep existing verified accounts usable.
            conn.execute(
                "UPDATE users
                 SET status = 'active',
                     approved_at = COALESCE(approved_at, created_at),
                     updated_at = COALESCE(updated_at, created_at)
                 WHERE status = 'pending'
                    OR (status = 'email_pending' AND email_verified = 1)",
                [],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn call<F, R>(&self, function: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Connection) -> tokio_rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.conns.len();
        self.conns[index]
            .call(function)
            .await
            .map_err(|error| anyhow::anyhow!("database error: {error}"))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ThemeStats {
    pub downloads: u64,
    pub heat: u64,
}

fn today() -> String {
    Utc::now().date_naive().format("%Y-%m-%d").to_string()
}

fn heat_cutoff() -> String {
    (Utc::now().date_naive() - ChronoDuration::days(HEAT_WINDOW_DAYS - 1))
        .format("%Y-%m-%d")
        .to_string()
}

fn heat_from(downloads: i64, recent: i64) -> u64 {
    let heat = downloads.max(0) + recent.max(0).saturating_mul(HEAT_RECENT_WEIGHT);
    heat.max(0) as u64
}

/// Read per-theme download totals and heat without touching the async pool.
///
/// Catalog rebuilds run from synchronous file code, so they open a short
/// read-only connection instead of plumbing the async database everywhere.
pub fn load_theme_stats(path: &Path) -> HashMap<String, ThemeStats> {
    let mut stats = HashMap::new();
    if !path.exists() {
        return stats;
    }
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return stats;
    };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let cutoff = heat_cutoff();
    let Ok(mut statement) = conn.prepare(
        "SELECT s.theme_id, s.downloads,
                s.downloads + ?2 * COALESCE((
                    SELECT SUM(d.count) FROM theme_download_days d
                    WHERE d.theme_id = s.theme_id AND d.day >= ?1
                ), 0) AS heat
         FROM theme_stats s",
    ) else {
        return stats;
    };
    let Ok(rows) = statement.query_map(params![cutoff, HEAT_RECENT_WEIGHT], |row| {
        let theme_id: String = row.get(0)?;
        let downloads: i64 = row.get(1)?;
        let heat: i64 = row.get(2)?;
        Ok((
            theme_id,
            ThemeStats {
                downloads: downloads.max(0) as u64,
                heat: heat.max(0) as u64,
            },
        ))
    }) else {
        return stats;
    };
    for row in rows.flatten() {
        stats.insert(row.0, row.1);
    }
    stats
}

/// Record a batch of package downloads in one SQLite transaction.
///
/// The request path only increments an in-memory buffer; this function is
/// called by the background flusher so high download concurrency does not
/// translate into one write transaction per package request.
pub async fn record_theme_downloads(
    db: &Database,
    counts: &HashMap<String, u64>,
) -> anyhow::Result<HashMap<String, ThemeStats>> {
    if counts.is_empty() {
        return Ok(HashMap::new());
    }
    let counts: Vec<(String, i64)> = counts
        .iter()
        .map(|(id, count)| (id.clone(), (*count).min(i64::MAX as u64) as i64))
        .collect();
    let day = today();
    let cutoff = heat_cutoff();
    let old_day = (Utc::now().date_naive() - ChronoDuration::days(90))
        .format("%Y-%m-%d")
        .to_string();
    let now = crate::util::now_iso();
    db.call(move |conn| {
        let transaction = conn.transaction()?;
        for (theme_id, count) in &counts {
            transaction.execute(
                "INSERT INTO theme_stats(theme_id, downloads, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(theme_id) DO UPDATE SET
                     downloads = downloads + ?2,
                     updated_at = excluded.updated_at",
                params![theme_id, count, now],
            )?;
            transaction.execute(
                "INSERT INTO theme_download_days(theme_id, day, count) VALUES (?1, ?2, ?3)
                 ON CONFLICT(theme_id, day) DO UPDATE SET count = count + ?3",
                params![theme_id, day, count],
            )?;
        }
        // Keep the daily table bounded; heat only reads the last week.
        transaction.execute(
            "DELETE FROM theme_download_days WHERE day < ?1",
            params![old_day],
        )?;
        let mut stats = HashMap::with_capacity(counts.len());
        for (theme_id, _) in &counts {
            let downloads: i64 = transaction.query_row(
                "SELECT downloads FROM theme_stats WHERE theme_id = ?1",
                params![theme_id],
                |row| row.get(0),
            )?;
            let recent: i64 = transaction.query_row(
                "SELECT COALESCE(SUM(count), 0) FROM theme_download_days
                 WHERE theme_id = ?1 AND day >= ?2",
                params![theme_id, cutoff],
                |row| row.get(0),
            )?;
            stats.insert(
                theme_id.clone(),
                ThemeStats {
                    downloads: downloads.max(0) as u64,
                    heat: heat_from(downloads, recent),
                },
            );
        }
        transaction.commit()?;
        Ok(stats)
    })
    .await
    .map_err(|error| anyhow::anyhow!("database error: {error}"))
}

/// Record one package download and return the theme's updated counters.
pub async fn record_theme_download(db: &Database, theme_id: &str) -> anyhow::Result<ThemeStats> {
    let mut counts = HashMap::new();
    counts.insert(theme_id.to_string(), 1);
    let mut stats = record_theme_downloads(db, &counts).await?;
    stats
        .remove(theme_id)
        .ok_or_else(|| anyhow::anyhow!("theme stats missing after update"))
}

/// Return `(all-time downloads, downloads in the recent window)`.
pub async fn download_totals(db: &Database) -> anyhow::Result<(u64, u64)> {
    let cutoff = heat_cutoff();
    db.call(move |conn| {
        let downloads: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(downloads), 0) FROM theme_stats",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let recent: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM theme_download_days WHERE day >= ?1",
                params![cutoff],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok((downloads.max(0) as u64, recent.max(0) as u64))
    })
    .await
    .map_err(|error| anyhow::anyhow!("database error: {error}"))
}

/// Daily download counts for the monitoring dashboard, oldest first.
pub async fn download_daily(db: &Database, days: i64) -> anyhow::Result<Vec<(String, u64)>> {
    let days = days.clamp(1, 90);
    let start = (Utc::now().date_naive() - ChronoDuration::days(days - 1))
        .format("%Y-%m-%d")
        .to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT day, SUM(count) FROM theme_download_days
             WHERE day >= ?1 GROUP BY day ORDER BY day ASC",
        )?;
        let mut rows = statement.query(params![start])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let day: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            items.push((day, count.max(0) as u64));
        }
        Ok(items)
    })
    .await
    .map_err(|error| anyhow::anyhow!("database error: {error}"))
}

pub fn get_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    let mut statement = conn.prepare("SELECT value FROM app_meta WHERE key = ?1")?;
    let mut rows = statement.query([key])?;
    Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO app_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_downloads_and_computes_heat() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stats.db");
        let db = Database::open(&path).await.unwrap();
        let first = record_theme_download(&db, "11010-01").await.unwrap();
        assert_eq!(first.downloads, 1);
        assert_eq!(first.heat, 1 + HEAT_RECENT_WEIGHT as u64);
        let second = record_theme_download(&db, "11010-01").await.unwrap();
        assert_eq!(second.downloads, 2);
        assert_eq!(second.heat, 2 + 2 * HEAT_RECENT_WEIGHT as u64);

        let loaded = load_theme_stats(&path);
        let stats = loaded.get("11010-01").expect("theme stats");
        assert_eq!(stats.downloads, 2);
        assert_eq!(stats.heat, 2 + 2 * HEAT_RECENT_WEIGHT as u64);

        let (total, recent) = download_totals(&db).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(recent, 2);
        let daily = download_daily(&db, 7).await.unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].1, 2);
    }

    #[tokio::test]
    async fn records_downloads_in_batches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("batch.db");
        let db = Database::open(&path).await.unwrap();
        let mut counts = HashMap::new();
        counts.insert("11010-01".to_string(), 3);
        counts.insert("11010-02".to_string(), 2);
        let first = record_theme_downloads(&db, &counts).await.unwrap();
        assert_eq!(first["11010-01"].downloads, 3);
        assert_eq!(first["11010-01"].heat, 3 + 3 * HEAT_RECENT_WEIGHT as u64);
        assert_eq!(first["11010-02"].downloads, 2);

        let second = record_theme_downloads(&db, &counts).await.unwrap();
        assert_eq!(second["11010-01"].downloads, 6);
        assert_eq!(second["11010-02"].downloads, 4);
        let (total, recent) = download_totals(&db).await.unwrap();
        assert_eq!(total, 10);
        assert_eq!(recent, 10);
    }

    #[test]
    fn missing_stats_database_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load_theme_stats(&directory.path().join("missing.db")).is_empty());
    }
}
