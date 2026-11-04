use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
PRAGMA wal_autocheckpoint=1000;
PRAGMA journal_size_limit=67108864;
PRAGMA mmap_size=33554432;
";

pub fn db_pool_size() -> usize {
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
const SPLIT_SCHEMA_VERSION: i64 = 2;

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
    ip TEXT,
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

/// Account domain. This is the ATTACH main database, so `users` stays the
/// default schema for account queries.
const ACCOUNT_SCHEMA: &str = r#"
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
    consent_version INTEGER NOT NULL DEFAULT 0,
    consent_at TEXT,
    updated_at TEXT NOT NULL,
    show_r18_set INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS theme_tokens (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    token_prefix TEXT NOT NULL,
    scopes TEXT NOT NULL DEFAULT 'read upload',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    expires_at INTEGER,
    last_used_at TEXT
);
CREATE TABLE IF NOT EXISTS app_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Authors and licenses. Cross-database foreign keys are intentionally
/// omitted; deletion is orchestrated by the application.
const AUTHORS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS authors (
    hash TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    display_name TEXT,
    user_id TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    auto_publish INTEGER NOT NULL DEFAULT 1,
    code TEXT UNIQUE,
    next_theme_seq INTEGER NOT NULL DEFAULT 1,
    approved_at TEXT,
    approved_by TEXT,
    last_used_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    auto_revoke_at TEXT
);
CREATE INDEX IF NOT EXISTS authors_user_idx ON authors(user_id);
CREATE INDEX IF NOT EXISTS authors_status_idx ON authors(status);
CREATE TABLE IF NOT EXISTS author_requests (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
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
CREATE TABLE IF NOT EXISTS author_restrictions (
    author_hash TEXT PRIMARY KEY,
    features TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    updated_by TEXT
);
"#;

/// Social graph and comments. Foreign keys are kept only inside this file
/// (comment replies and mentions); user/author references are cross-database.
const SOCIAL_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS theme_comments (
    id TEXT PRIMARY KEY,
    theme_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
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
    user_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (comment_id, user_id)
);
CREATE INDEX IF NOT EXISTS theme_comment_mentions_user_idx
    ON theme_comment_mentions(user_id);
CREATE TABLE IF NOT EXISTS user_follows (
    follower_id TEXT NOT NULL,
    author_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (follower_id, author_hash)
);
CREATE INDEX IF NOT EXISTS user_follows_author_idx
    ON user_follows(author_hash);
"#;

const STATS_SCHEMA: &str = r#"
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
"#;

const AUDIT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id TEXT,
    action TEXT NOT NULL,
    target_type TEXT,
    target_id TEXT,
    detail TEXT,
    ip TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_log_created_idx ON audit_log(created_at);
CREATE INDEX IF NOT EXISTS audit_log_actor_idx ON audit_log(actor_id, created_at);
"#;

#[derive(Clone, Copy, Debug)]
pub enum DbKind {
    Account,
    Authors,
    Social,
    Stats,
    Audit,
}

impl DbKind {
    fn schema(self) -> &'static str {
        match self {
            DbKind::Account => ACCOUNT_SCHEMA,
            DbKind::Authors => AUTHORS_SCHEMA,
            DbKind::Social => SOCIAL_SCHEMA,
            DbKind::Stats => STATS_SCHEMA,
            DbKind::Audit => AUDIT_SCHEMA,
        }
    }
}

fn create_schema_file(path: &Path, kind: DbKind) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建数据库目录 {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let conn = Connection::open(path)
        .with_context(|| format!("无法打开数据库 {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-8000;",
    )?;
    conn.execute_batch(kind.schema())?;
    if matches!(kind, DbKind::Audit) {
        let has_ip: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('audit_log') WHERE name = 'ip'",
            [],
            |row| row.get(0),
        )?;
        if has_ip == 0 {
            conn.execute("ALTER TABLE audit_log ADD COLUMN ip TEXT", [])?;
        }
    }
    conn.pragma_update(None, "user_version", SPLIT_SCHEMA_VERSION)?;
    drop(conn);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn ensure_split_schema(paths: &crate::config::Paths) -> anyhow::Result<()> {
    for (path, kind) in [
        (&paths.db, DbKind::Account),
        (&paths.authors_db, DbKind::Authors),
        (&paths.social_db, DbKind::Social),
        (&paths.stats_db, DbKind::Stats),
        (&paths.audit_db, DbKind::Audit),
    ] {
        create_schema_file(path, kind)?;
    }
    Ok(())
}

fn table_columns(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> anyhow::Result<Vec<String>> {
    let escaped = table.replace('\'', "''");
    let mut statement =
        conn.prepare(&format!("PRAGMA {schema}.table_info('{escaped}')"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns)
}

#[derive(Debug)]
pub struct MigrationReport {
    pub target: String,
    pub tables: Vec<(String, u64)>,
}

/// Copies the legacy single-file database into `db-split/` without modifying
/// the legacy file. The caller must stop the service before running this.
pub fn migrate_split(paths: &crate::config::Paths) -> anyhow::Result<MigrationReport> {
    if paths.db_layout == crate::config::DbLayout::Split {
        anyhow::bail!("数据库已经是分库布局");
    }
    if !paths.legacy_db.is_file() {
        if paths.db_dir.exists() {
            let non_empty = std::fs::read_dir(&paths.db_dir)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
            if non_empty {
                anyhow::bail!(
                    "旧数据库不存在，但目标目录已存在且非空: {}",
                    paths.db_dir.display()
                );
            }
        }
        std::fs::create_dir_all(&paths.db_dir)
            .with_context(|| format!("无法创建目录 {}", paths.db_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &paths.db_dir,
                std::fs::Permissions::from_mode(0o700),
            );
        }
        for (name, kind) in [
            ("account.db", DbKind::Account),
            ("authors.db", DbKind::Authors),
            ("social.db", DbKind::Social),
            ("stats.db", DbKind::Stats),
            ("audit.db", DbKind::Audit),
        ] {
            create_schema_file(&paths.db_dir.join(name), kind)?;
        }
        crate::config::set_env_key(&paths.admin_config, "THEME_DB_LAYOUT", "split")?;
        crate::config::set_env_key(
            &paths.admin_config,
            "THEME_DB_DIR",
            &paths.db_dir.to_string_lossy(),
        )?;
        return Ok(MigrationReport {
            target: paths.db_dir.to_string_lossy().to_string(),
            tables: Vec::new(),
        });
    }
    if paths.db_dir.exists() {
        let non_empty = std::fs::read_dir(&paths.db_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if non_empty {
            anyhow::bail!(
                "目标目录已存在且非空: {}，请先移走或备份",
                paths.db_dir.display()
            );
        }
    }
    std::fs::create_dir_all(&paths.db_dir)
        .with_context(|| format!("无法创建目录 {}", paths.db_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &paths.db_dir,
            std::fs::Permissions::from_mode(0o700),
        );
    }

    let target_account = paths.db_dir.join("account.db");
    let target_authors = paths.db_dir.join("authors.db");
    let target_social = paths.db_dir.join("social.db");
    let target_stats = paths.db_dir.join("stats.db");
    let target_audit = paths.db_dir.join("audit.db");
    for (path, kind) in [
        (&target_account, DbKind::Account),
        (&target_authors, DbKind::Authors),
        (&target_social, DbKind::Social),
        (&target_stats, DbKind::Stats),
        (&target_audit, DbKind::Audit),
    ] {
        create_schema_file(path, kind)?;
    }

    let legacy = Connection::open(&paths.legacy_db)
        .with_context(|| format!("无法打开旧数据库 {}", paths.legacy_db.display()))?;
    legacy.execute_batch("PRAGMA foreign_keys=OFF;")?;
    for (alias, path) in [
        ("new_account", &target_account),
        ("authors", &target_authors),
        ("social", &target_social),
        ("stats", &target_stats),
        ("audit", &target_audit),
    ] {
        let escaped = path.to_string_lossy().replace('\'', "''");
        legacy.execute(&format!("ATTACH DATABASE '{escaped}' AS {alias}"), [])?;
    }

    let copy_plan: [(&str, &[&str]); 5] = [
        (
            "new_account",
            &[
                "users",
                "user_identities",
                "sessions",
                "email_tokens",
                "oauth_states",
                "device_login_codes",
                "user_preferences",
                "theme_tokens",
                "app_meta",
            ],
        ),
        (
            "authors",
            &[
                "authors",
                "author_requests",
                "author_reset_history",
                "submissions",
                "author_restrictions",
            ],
        ),
        (
            "social",
            &["user_follows", "theme_comments", "theme_comment_mentions"],
        ),
        ("stats", &["theme_stats", "theme_download_days"]),
        ("audit", &["audit_log"]),
    ];
    let mut report_tables = Vec::new();
    for (alias, tables) in copy_plan {
        for table in tables {
            let exists: i64 = legacy.query_row(
                "SELECT COUNT(*) FROM main.sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )?;
            if exists == 0 {
                continue;
            }
            let count: i64 = legacy.query_row(
                &format!("SELECT COUNT(*) FROM main.\"{table}\""),
                [],
                |row| row.get(0),
            )?;
            let target_columns = table_columns(&legacy, alias, table)?;
            let legacy_columns = table_columns(&legacy, "main", table)?;
            let common: Vec<String> = target_columns
                .into_iter()
                .filter(|column| legacy_columns.contains(column))
                .collect();
            if common.is_empty() {
                anyhow::bail!("表 {table} 在旧库和新库之间没有共同列");
            }
            let column_list = common
                .iter()
                .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", ");
            legacy.execute(
                &format!(
                    "INSERT INTO {alias}.\"{table}\" ({column_list})
                     SELECT {column_list} FROM main.\"{table}\""
                ),
                [],
            )?;
            let copied: i64 = legacy.query_row(
                &format!("SELECT COUNT(*) FROM {alias}.\"{table}\""),
                [],
                |row| row.get(0),
            )?;
            if copied != count {
                anyhow::bail!(
                    "表 {table} 复制数量不一致: {count} -> {copied}"
                );
            }
            report_tables.push((table.to_string(), copied.max(0) as u64));
        }
    }

    for alias in ["new_account", "authors", "social", "stats", "audit"] {
        let check: String =
            legacy.query_row(&format!("PRAGMA {alias}.integrity_check"), [], |row| row.get(0))?;
        if check.to_ascii_lowercase() != "ok" {
            anyhow::bail!("{alias} 完整性检查失败: {check}");
        }
    }
    for alias in ["new_account", "authors", "social", "stats", "audit"] {
        let _ = legacy.execute(&format!("DETACH DATABASE {alias}"), []);
    }
    drop(legacy);

    crate::config::set_env_key(
        &paths.admin_config,
        "THEME_DB_LAYOUT",
        "split",
    )?;
    crate::config::set_env_key(
        &paths.admin_config,
        "THEME_DB_DIR",
        &paths.db_dir.to_string_lossy(),
    )?;
    Ok(MigrationReport {
        target: paths.db_dir.to_string_lossy().to_string(),
        tables: report_tables,
    })
}

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

    /// Opens the split layout and attaches the four side databases on every
    /// pooled connection. SQL can still JOIN across files; writes to different
    /// files use independent SQLite write locks.
    pub async fn open_split(
        main: &Path,
        attachments: &[(&str, PathBuf)],
        pool_size: usize,
    ) -> anyhow::Result<Self> {
        let pool_size = pool_size.clamp(1, 32);
        let attachments: Vec<(String, PathBuf)> = attachments
            .iter()
            .map(|(alias, path)| (alias.to_string(), path.clone()))
            .collect();
        let mut conns = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let conn = AsyncConnection::open(main)
                .await
                .with_context(|| format!("无法打开数据库 {}", main.display()))?;
            let attachments = attachments.clone();
            conn.call(move |conn| {
                conn.execute_batch(CONNECTION_PRAGMAS)?;
                for (alias, path) in &attachments {
                    let escaped = path.to_string_lossy().replace('\'', "''");
                    conn.execute(
                        &format!("ATTACH DATABASE '{escaped}' AS {alias}"),
                        [],
                    )?;
                    conn.execute_batch(&format!(
                        "PRAGMA {alias}.journal_mode=WAL;
                         PRAGMA {alias}.cache_size=-8000;"
                    ))?;
                }
                Ok(())
            })
            .await
            .with_context(|| format!("无法挂载数据库 {}", main.display()))?;
            conns.push(conn);
        }
        let db = Self {
            conns: Arc::new(conns),
            next: Arc::new(AtomicUsize::new(0)),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(main, std::fs::Permissions::from_mode(0o600));
        }
        Ok(db)
    }

    #[cfg(test)]
    pub async fn open_test(root: &Path) -> anyhow::Result<Self> {
        let dir = root.join("db-split");
        let paths = [
            (dir.join("account.db"), DbKind::Account),
            (dir.join("authors.db"), DbKind::Authors),
            (dir.join("social.db"), DbKind::Social),
            (dir.join("stats.db"), DbKind::Stats),
            (dir.join("audit.db"), DbKind::Audit),
        ];
        for (path, kind) in &paths {
            create_schema_file(path, *kind)?;
        }
        let attachments = [
            ("authors", dir.join("authors.db")),
            ("social", dir.join("social.db")),
            ("stats", dir.join("stats.db")),
            ("audit", dir.join("audit.db")),
        ];
        Self::open_split(&dir.join("account.db"), &attachments, 2).await
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
                "CREATE INDEX IF NOT EXISTS audit_log_actor_idx
                    ON audit_log(actor_id, created_at);
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
                );
                CREATE TABLE IF NOT EXISTS theme_tokens (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    token_hash TEXT NOT NULL UNIQUE,
                    token_prefix TEXT NOT NULL,
                    scopes TEXT NOT NULL DEFAULT 'read upload',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL,
                    expires_at INTEGER,
                    last_used_at TEXT
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
            let has_audit_ip: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('audit_log') WHERE name = 'ip'",
                [],
                |row| row.get(0),
            )?;
            if has_audit_ip == 0 {
                conn.execute("ALTER TABLE audit_log ADD COLUMN ip TEXT", [])?;
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

    pub async fn optimize(&self) -> anyhow::Result<()> {
        self.call(|conn| {
            conn.execute_batch("PRAGMA optimize;")?;
            Ok(())
        })
        .await
    }

    /// Cross-file foreign keys are enforced by the application. This removes
    /// rows left behind by a failed multi-step delete or an older import.
    pub async fn repair_orphans(&self) -> anyhow::Result<usize> {
        self.call(|conn| {
            let mut changed = 0;
            for sql in [
                "DELETE FROM social.theme_comment_mentions
                 WHERE user_id NOT IN (SELECT id FROM users)",
                "DELETE FROM social.theme_comments
                 WHERE user_id NOT IN (SELECT id FROM users)",
                "DELETE FROM social.user_follows
                 WHERE follower_id NOT IN (SELECT id FROM users)
                    OR author_hash NOT IN (SELECT hash FROM authors.authors)",
                "UPDATE authors.authors SET user_id = NULL
                 WHERE user_id IS NOT NULL
                   AND user_id NOT IN (SELECT id FROM users)",
                "DELETE FROM authors.author_requests
                 WHERE user_id NOT IN (SELECT id FROM users)",
                "DELETE FROM authors.author_restrictions
                 WHERE author_hash NOT IN (SELECT hash FROM authors.authors)",
                "DELETE FROM authors.author_reset_history
                 WHERE user_id NOT IN (SELECT id FROM users)",
                "UPDATE authors.submissions SET user_id = NULL
                 WHERE user_id IS NOT NULL
                   AND user_id NOT IN (SELECT id FROM users)",
            ] {
                changed += conn.execute(sql, [])?;
            }
            Ok(changed)
        })
        .await
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
                "INSERT INTO stats.theme_stats(theme_id, downloads, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(theme_id) DO UPDATE SET
                     downloads = downloads + ?2,
                     updated_at = excluded.updated_at",
                params![theme_id, count, now],
            )?;
            transaction.execute(
                "INSERT INTO stats.theme_download_days(theme_id, day, count) VALUES (?1, ?2, ?3)
                 ON CONFLICT(theme_id, day) DO UPDATE SET count = count + ?3",
                params![theme_id, day, count],
            )?;
        }
        // Keep the daily table bounded; heat only reads the last week.
        transaction.execute(
            "DELETE FROM stats.theme_download_days WHERE day < ?1",
            params![old_day],
        )?;
        let mut stats = HashMap::with_capacity(counts.len());
        for (theme_id, _) in &counts {
            let downloads: i64 = transaction.query_row(
                "SELECT downloads FROM stats.theme_stats WHERE theme_id = ?1",
                params![theme_id],
                |row| row.get(0),
            )?;
            let recent: i64 = transaction.query_row(
                "SELECT COALESCE(SUM(count), 0) FROM stats.theme_download_days
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
                "SELECT COALESCE(SUM(downloads), 0) FROM stats.theme_stats",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let recent: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(count), 0) FROM stats.theme_download_days WHERE day >= ?1",
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
            "SELECT day, SUM(count) FROM stats.theme_download_days
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
        let root = directory.path();
        let path = root.join("db-split/stats.db");
        let db = Database::open_test(root).await.unwrap();
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
        let db = Database::open_test(directory.path()).await.unwrap();
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
    fn initializes_empty_split_layout_on_fresh_install() {
        use crate::config::{DbLayout, Paths};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let legacy = root.join("account.db");
        let paths = Paths {
            root: root.to_path_buf(),
            meta: root.join("meta"),
            pending: root.join("pending"),
            uploads: root.join("uploads"),
            catalog: root.join("catalog.json"),
            static_dir: root.join("static"),
            authors_file: root.join("authors.json"),
            token_env: root.join("token.env"),
            admin_env: root.join("admin.env"),
            admin_password_file: root.join("admin-password"),
            admin_config: root.join("config.env"),
            storage_env: root.join("storage.env"),
            account_env: root.join("account.env"),
            network_env: root.join("network.env"),
            sources_file: root.join("sources.json"),
            legacy_db: legacy.clone(),
            db_dir: root.join("db-split"),
            db_layout: DbLayout::Single,
            db: legacy.clone(),
            authors_db: legacy.clone(),
            social_db: legacy.clone(),
            stats_db: legacy.clone(),
            audit_db: legacy.clone(),
        };
        let report = migrate_split(&paths).unwrap();
        assert!(report.tables.is_empty());
        for name in [
            "account.db",
            "authors.db",
            "social.db",
            "stats.db",
            "audit.db",
        ] {
            assert!(root.join("db-split").join(name).is_file(), "{name}");
        }
        let config = std::fs::read_to_string(root.join("config.env")).unwrap();
        assert!(config.contains("THEME_DB_LAYOUT=split"));
    }

    #[tokio::test]
    async fn repairs_cross_database_orphans() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::open_test(directory.path()).await.unwrap();
        db.call(|conn| {
            conn.execute(
                "INSERT INTO social.user_follows(follower_id, author_hash, created_at)
                 VALUES ('missing-user', 'missing-author', '2026-01-01T00:00:00Z')",
                [],
            )?;
            conn.execute(
                "INSERT INTO social.theme_comments(id, theme_id, user_id, content, created_at, updated_at)
                 VALUES ('c1', '11010-01', 'missing-user', 'hi', '2026-01-01T00:00:00Z',
                         '2026-01-01T00:00:00Z')",
                [],
            )?;
            conn.execute(
                "INSERT INTO authors.authors(hash, fingerprint, display_name, user_id, status,
                                             created_at, updated_at)
                 VALUES ('orphan-author', 'fp', 'Orphan', 'missing-user', 'active',
                         '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(db.repair_orphans().await.unwrap() > 0);
        let counts: (i64, i64, i64) = db
            .call(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM social.user_follows", [], |row| {
                        row.get(0)
                    })?,
                    conn.query_row("SELECT COUNT(*) FROM social.theme_comments", [], |row| {
                        row.get(0)
                    })?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM authors.authors WHERE user_id IS NOT NULL",
                        [],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(counts, (0, 0, 0));
    }

    #[test]
    fn missing_stats_database_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load_theme_stats(&directory.path().join("missing.db")).is_empty());
    }

    #[tokio::test]
    async fn migrates_legacy_database_and_verifies_counts() {
        use crate::config::{DbLayout, Paths};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let legacy = root.join("legacy.db");
        let legacy_db = Database::open(&legacy).await.unwrap();
        legacy_db
            .call(|conn| {
                conn.execute(
                    "INSERT INTO users(id, username, display_name, status, created_at, updated_at)
                     VALUES ('u1', 'alice', 'Alice', 'active', '2026-01-01T00:00:00Z',
                             '2026-01-01T00:00:00Z')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO authors(hash, fingerprint, display_name, user_id, status,
                                         created_at, updated_at)
                     VALUES ('h1', 'fingerprint', 'Alice', 'u1', 'active',
                             '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO user_follows(follower_id, author_hash, created_at)
                     VALUES ('u1', 'h1', '2026-01-01T00:00:00Z')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO theme_comments(id, theme_id, user_id, content, created_at, updated_at)
                     VALUES ('c1', '11010-01', 'u1', 'hello', '2026-01-01T00:00:00Z',
                             '2026-01-01T00:00:00Z')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO theme_stats(theme_id, downloads, updated_at)
                     VALUES ('11010-01', 3, '2026-01-01T00:00:00Z')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO audit_log(actor_id, action, created_at)
                     VALUES ('u1', 'test.action', '2026-01-01T00:00:00Z')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let paths = Paths {
            root: root.to_path_buf(),
            meta: root.join("meta"),
            pending: root.join("pending"),
            uploads: root.join("uploads"),
            catalog: root.join("catalog.json"),
            static_dir: root.join("static"),
            authors_file: root.join("authors.json"),
            token_env: root.join("token.env"),
            admin_env: root.join("admin.env"),
            admin_password_file: root.join("admin-password"),
            admin_config: root.join("config.env"),
            storage_env: root.join("storage.env"),
            account_env: root.join("account.env"),
            network_env: root.join("network.env"),
            sources_file: root.join("sources.json"),
            legacy_db: legacy.clone(),
            db_dir: root.join("db-split"),
            db_layout: DbLayout::Single,
            db: legacy.clone(),
            authors_db: legacy.clone(),
            social_db: legacy.clone(),
            stats_db: legacy.clone(),
            audit_db: legacy.clone(),
        };
        let report = migrate_split(&paths).unwrap();
        assert_eq!(report.target, root.join("db-split").to_string_lossy());
        assert!(root.join("db-split/account.db").is_file());
        assert!(root.join("db-split/authors.db").is_file());
        assert!(root.join("db-split/social.db").is_file());
        assert!(root.join("db-split/stats.db").is_file());
        assert!(root.join("db-split/audit.db").is_file());
        for (path, table, expected) in [
            (root.join("db-split/account.db"), "users", 1),
            (root.join("db-split/authors.db"), "authors", 1),
            (root.join("db-split/social.db"), "user_follows", 1),
            (root.join("db-split/social.db"), "theme_comments", 1),
            (root.join("db-split/stats.db"), "theme_stats", 1),
            (root.join("db-split/audit.db"), "audit_log", 1),
        ] {
            let conn = Connection::open(path).unwrap();
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, expected, "{table}");
        }
        let config = std::fs::read_to_string(root.join("config.env")).unwrap();
        assert!(config.contains("THEME_DB_LAYOUT=split"));
    }
}
