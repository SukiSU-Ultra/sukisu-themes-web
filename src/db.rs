use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rusqlite::Connection;
use tokio_rusqlite::Connection as AsyncConnection;

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
"#;

#[derive(Clone)]
pub struct Database {
    conn: Arc<AsyncConnection>,
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
        let conn = AsyncConnection::open(path)
            .await
            .with_context(|| format!("无法打开数据库 {}", path.display()))?;
        let db = Self {
            conn: Arc::new(conn),
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
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;\nPRAGMA foreign_keys=ON;\nPRAGMA busy_timeout=5000;",
            )?;
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version < 1 {
                conn.execute_batch(SCHEMA_V1)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
        self.conn
            .call(function)
            .await
            .map_err(|error| anyhow::anyhow!("database error: {error}"))
    }
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
