use std::path::Path;

use anyhow::{anyhow, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{read_env, Paths};
use crate::db::Database;
use crate::util::{now_iso, random_hex, unix_now};

pub const EMAIL_VERIFY_TTL_SECS: u64 = 24 * 60 * 60;
pub const PASSWORD_RESET_TTL_SECS: u64 = 30 * 60;
pub const ANDROID_SESSION_TTL_SECS: u64 = 30 * 24 * 60 * 60;

const USER_COLUMNS: &str = "id, username, display_name, email, avatar, role, status, \
    email_verified, rejection_reason, created_at, updated_at, approved_at, last_login_at, is_super_admin";

#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub avatar: Option<String>,
    pub role: String,
    pub status: String,
    pub email_verified: bool,
    pub rejection_reason: Option<String>,
    pub created_at: String,
    pub approved_at: Option<String>,
    pub last_login_at: Option<String>,
    pub is_super_admin: bool,
}

impl User {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            username: row.get(1)?,
            display_name: row.get(2)?,
            email: row.get(3)?,
            avatar: row.get(4)?,
            role: row.get(5)?,
            status: row.get(6)?,
            email_verified: row.get::<_, i64>(7)? != 0,
            rejection_reason: row.get(8)?,
            created_at: row.get(9)?,
            approved_at: row.get(11)?,
            last_login_at: row.get(12)?,
            is_super_admin: row.get::<_, i64>(13)? != 0,
        })
    }

    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    pub fn is_active(&self) -> bool {
        self.status == "active"
    }

    pub fn can_login(&self) -> bool {
        matches!(self.status.as_str(), "email_pending" | "pending" | "active")
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub status: String,
    pub email_verified: bool,
    pub scope: String,
}

pub async fn user_by_id(db: &Database, id: &str) -> Result<Option<User>> {
    let id = id.to_string();
    db.call(move |conn| {
        let mut statement =
            conn.prepare(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"))?;
        let mut rows = statement.query([id])?;
        Ok(rows.next()?.map(User::from_row).transpose()?)
    })
    .await
}

pub async fn user_by_login(db: &Database, login: &str) -> Result<Option<User>> {
    let login = login.trim().to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {USER_COLUMNS} FROM users
             WHERE username = ?1 COLLATE NOCASE OR email = ?1 COLLATE NOCASE
             LIMIT 1"
        ))?;
        let mut rows = statement.query([login])?;
        Ok(rows.next()?.map(User::from_row).transpose()?)
    })
    .await
}

pub async fn user_by_email(db: &Database, email: &str) -> Result<Option<User>> {
    let email = email.trim().to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {USER_COLUMNS} FROM users WHERE email = ?1 COLLATE NOCASE LIMIT 1"
        ))?;
        let mut rows = statement.query([email])?;
        Ok(rows.next()?.map(User::from_row).transpose()?)
    })
    .await
}

pub async fn username_exists(db: &Database, username: &str) -> Result<bool> {
    let username = username.trim().to_string();
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE username = ?1 COLLATE NOCASE",
            [username],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    })
    .await
}

pub async fn user_password_hash(db: &Database, user_id: &str) -> Result<Option<String>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let value: Option<String> = conn
            .query_row(
                "SELECT password_hash FROM users WHERE id = ?1",
                [user_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(value)
    })
    .await
}

pub async fn email_exists(db: &Database, email: &str) -> Result<bool> {
    let email = email.trim().to_string();
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE email = ?1 COLLATE NOCASE",
            [email],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    })
    .await
}

pub struct NewUser<'a> {
    pub username: &'a str,
    pub display_name: &'a str,
    pub email: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub role: &'a str,
    pub status: &'a str,
    pub email_verified: bool,
}

pub async fn create_user(db: &Database, new_user: NewUser<'_>) -> Result<User> {
    let id = random_hex(16);
    let now = now_iso();
    let username = new_user.username.trim().to_string();
    let display_name = new_user.display_name.trim().to_string();
    let email = new_user
        .email
        .map(|value| value.trim().to_ascii_lowercase());
    let password_hash = new_user.password_hash.map(str::to_string);
    let role = new_user.role.to_string();
    let status = new_user.status.to_string();
    let email_verified = new_user.email_verified;
    let insert_id = id.clone();
    db.call(move |conn| {
        if let Some(email) = &email {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM users WHERE email = ?1 COLLATE NOCASE",
                [email],
                |row| row.get(0),
            )?;
            if exists > 0 {
                return Err(tokio_rusqlite::Error::Rusqlite(
                    rusqlite::Error::InvalidQuery,
                ));
            }
        }
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE username = ?1 COLLATE NOCASE",
            [&username],
            |row| row.get(0),
        )?;
        if exists > 0 {
            return Err(tokio_rusqlite::Error::Rusqlite(
                rusqlite::Error::InvalidQuery,
            ));
        }
        conn.execute(
            "INSERT INTO users
             (id, username, display_name, email, password_hash, role, status,
              email_verified, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                insert_id,
                username,
                display_name,
                email,
                password_hash,
                role,
                status,
                if email_verified { 1 } else { 0 },
                now,
            ],
        )?;
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("用户名或邮箱已被使用"))?;
    user_by_id(db, &id)
        .await?
        .ok_or_else(|| anyhow!("创建用户后读取失败"))
}

pub async fn mark_email_verified(db: &Database, user_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE users
             SET email_verified = 1,
                 status = CASE
                     WHEN status IN ('email_pending', 'pending') THEN 'active'
                     ELSE status
                 END,
                 updated_at = ?2
             WHERE id = ?1",
            params![user_id, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn update_login_time(db: &Database, user_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
            params![user_id, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_status(
    db: &Database,
    user_id: &str,
    status: &str,
    reason: Option<&str>,
    actor: Option<&str>,
) -> Result<()> {
    let user_id = user_id.to_string();
    let status = status.to_string();
    let reason = reason.map(str::to_string);
    let actor = actor.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        let approved_at = if status == "active" {
            Some(now.clone())
        } else {
            None
        };
        conn.execute(
            "UPDATE users
             SET status = ?2,
                 rejection_reason = ?3,
                 approved_at = COALESCE(?4, approved_at),
                 approved_by = COALESCE(?5, approved_by),
                 updated_at = ?6
             WHERE id = ?1",
            params![user_id, status, reason, approved_at, actor, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete_user(db: &Database, user_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        conn.execute("DELETE FROM social.theme_favorites WHERE user_id = ?1", [&user_id])?;
        conn.execute("DELETE FROM social.theme_history WHERE user_id = ?1", [&user_id])?;
        conn.execute("DELETE FROM social.theme_update_reads WHERE user_id = ?1", [&user_id])?;
        conn.execute("DELETE FROM social.theme_appeals WHERE target_kind = 'report' AND target_id IN (SELECT id FROM social.theme_reports WHERE reporter_id = ?1)", [&user_id])?;
        conn.execute("DELETE FROM social.theme_reports WHERE reporter_id = ?1", [&user_id])?;
        conn.execute("DELETE FROM social.theme_appeals WHERE appellant_id = ?1", [&user_id])?;
        conn.execute("DELETE FROM authors.submission_drafts WHERE user_id = ?1", [&user_id])?;
        conn.execute("DELETE FROM email_tokens WHERE user_id = ?1", [&user_id])?;
        conn.execute(
            "DELETE FROM oauth_states WHERE user_id = ?1 OR link_user_id = ?1",
            [&user_id],
        )?;
        conn.execute("DELETE FROM users WHERE id = ?1", [&user_id])?;
        Ok(())
    })
    .await
}

pub async fn set_role(db: &Database, user_id: &str, role: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let role = role.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE users SET role = ?2, updated_at = ?3 WHERE id = ?1",
            params![user_id, role, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn update_profile(
    db: &Database,
    user_id: &str,
    username: &str,
    display_name: &str,
    avatar: Option<&str>,
) -> Result<()> {
    let user_id = user_id.to_string();
    let username = username.trim().to_string();
    let display_name = display_name.trim().to_string();
    let avatar = avatar.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE username = ?1 COLLATE NOCASE AND id <> ?2",
            params![username, user_id],
            |row| row.get(0),
        )?;
        if exists > 0 {
            return Err(tokio_rusqlite::Error::Rusqlite(rusqlite::Error::InvalidQuery));
        }
        if let Some(avatar) = avatar {
            conn.execute(
                "UPDATE users SET username = ?2, display_name = ?3, avatar = ?4, updated_at = ?5 WHERE id = ?1",
                params![user_id, username, display_name, avatar, now],
            )?;
        } else {
            conn.execute(
                "UPDATE users SET username = ?2, display_name = ?3, updated_at = ?4 WHERE id = ?1",
                params![user_id, username, display_name, now],
            )?;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("用户名已被使用"))
}

pub async fn update_password(db: &Database, user_id: &str, hash: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let hash = hash.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE users SET password_hash = ?2, updated_at = ?3 WHERE id = ?1",
            params![user_id, hash, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn update_email(db: &Database, user_id: &str, email: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let email = email.trim().to_ascii_lowercase();
    let now = now_iso();
    db.call(move |conn| {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE email = ?1 COLLATE NOCASE AND id <> ?2",
            params![email, user_id],
            |row| row.get(0),
        )?;
        if exists > 0 {
            return Err(tokio_rusqlite::Error::Rusqlite(
                rusqlite::Error::InvalidQuery,
            ));
        }
        conn.execute(
            "UPDATE users SET email = ?2, email_verified = 1, updated_at = ?3 WHERE id = ?1",
            params![user_id, email, now],
        )?;
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("邮箱已被使用"))
}

pub async fn set_avatar(db: &Database, user_id: &str, avatar: Option<&str>) -> Result<()> {
    let user_id = user_id.to_string();
    let avatar = avatar.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE users SET avatar = ?2, updated_at = ?3 WHERE id = ?1",
            params![user_id, avatar, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn list_users(db: &Database, status: Option<&str>, query: &str) -> Result<Vec<User>> {
    let status = status.map(str::to_string);
    let query = query.trim().to_string();
    db.call(move |conn| {
        let mut sql = format!("SELECT {USER_COLUMNS} FROM users WHERE 1 = 1");
        let mut values: Vec<String> = Vec::new();
        if let Some(status) = status {
            if status == "pending" {
                sql.push_str(" AND status IN ('email_pending', 'pending')");
            } else {
                sql.push_str(" AND status = ?");
                values.push(status);
            }
        }
        if !query.is_empty() {
            sql.push_str(" AND (username LIKE ? OR display_name LIKE ? OR email LIKE ?)");
            let pattern = format!("%{query}%");
            values.push(pattern.clone());
            values.push(pattern.clone());
            values.push(pattern);
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT 500");
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(values.iter()))?;
        let mut users = Vec::new();
        while let Some(row) = rows.next()? {
            users.push(User::from_row(row)?);
        }
        Ok(users)
    })
    .await
}

pub async fn admin_username_hint(db: &Database) -> Result<Option<String>> {
    db.call(|conn| {
        Ok(conn
            .query_row(
                "SELECT username FROM users WHERE role = 'admin' AND status <> 'disabled'
                 ORDER BY created_at ASC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    })
    .await
}

pub async fn count_users(db: &Database) -> Result<i64> {
    db.call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?))
        .await
}

pub async fn count_pending_users(db: &Database) -> Result<i64> {
    db.call(|conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM users WHERE status IN ('email_pending', 'pending')",
            [],
            |row| row.get(0),
        )?)
    })
    .await
}

pub async fn count_disabled_users(db: &Database) -> Result<i64> {
    db.call(|conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM users WHERE status IN ('disabled', 'rejected')",
            [],
            |row| row.get(0),
        )?)
    })
    .await
}

pub async fn admin_count(db: &Database) -> Result<i64> {
    db.call(|conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND status <> 'disabled'",
            [],
            |row| row.get(0),
        )?)
    })
    .await
}

pub async fn is_super_admin(db: &Database, user_id: &str) -> Result<bool> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT is_super_admin FROM users WHERE id = ?1",
                params![user_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0)
    })
    .await
}

pub async fn mark_super_admin(db: &Database, user_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        conn.execute(
            "UPDATE users SET is_super_admin = CASE WHEN id = ?1 THEN 1 ELSE 0 END",
            params![user_id],
        )?;
        Ok(())
    })
    .await
}

/// Older databases predate the super-admin flag. Mark the configured admin
/// (or the earliest admin) once so promoted admins remain distinguishable.
pub async fn ensure_super_admin(db: &Database, paths: &Paths) -> Result<bool> {
    let has_super: i64 = db
        .call(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM users WHERE is_super_admin = 1",
                [],
                |row| row.get(0),
            )?)
        })
        .await?;
    if has_super > 0 {
        return Ok(false);
    }
    let configured = crate::auth::configured_username(paths);
    let target = match user_by_login(db, &configured).await? {
        Some(user) if user.role == "admin" => Some(user.id),
        _ => {
            db.call(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT id FROM users WHERE role = 'admin'
                         ORDER BY created_at ASC, id ASC LIMIT 1",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await?
        }
    };
    if let Some(id) = target {
        mark_super_admin(db, &id).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn ensure_admin_seed(db: &Database, paths: &Paths) -> Result<bool> {
    if admin_count(db).await? > 0 {
        return Ok(false);
    }
    let env = read_env(&paths.admin_env);
    let hash = env
        .get("ADMIN_PASSWORD_HASH")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(hash) = hash else {
        return Ok(false);
    };
    let username = env
        .get("ADMIN_USERNAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(crate::auth::admin_username);
    let user = create_user(
        db,
        NewUser {
            username: &username,
            display_name: &username,
            email: None,
            password_hash: Some(&hash),
            role: "admin",
            status: "active",
            email_verified: true,
        },
    )
    .await?;
    mark_super_admin(db, &user.id).await?;
    Ok(true)
}

pub async fn audit(
    db: &Database,
    actor_id: Option<&str>,
    action: &str,
    target_type: Option<&str>,
    target_id: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    let actor_id = actor_id.map(str::to_string);
    let action = action.to_string();
    let target_type = target_type.map(str::to_string);
    let target_id = target_id.map(str::to_string);
    let detail = detail.map(str::to_string);
    let ip = crate::api::current_request_ip();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO audit.audit_log(actor_id, action, target_type, target_id, detail, ip, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![actor_id, action, target_type, target_id, detail, ip, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn create_session(
    db: &Database,
    user_id: &str,
    scope: &str,
    ttl_secs: u64,
    user_agent: &str,
    ip: &str,
) -> Result<String> {
    let token = random_hex(32);
    let token_hash = hash_token(&token);
    let user_id = user_id.to_string();
    let scope = scope.to_string();
    let user_agent = user_agent.to_string();
    let ip = ip.to_string();
    let now = now_iso();
    let now_unix = unix_now() as i64;
    let expires_at = now_unix + ttl_secs as i64;
    db.call(move |conn| {
        conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now_unix])?;
        conn.execute(
            "INSERT INTO sessions(token_hash, user_id, scope, expires_at, created_at, last_seen_at, user_agent, ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
            params![token_hash, user_id, scope, expires_at, now, user_agent, ip],
        )?;
        Ok(())
    })
    .await?;
    Ok(token)
}

pub async fn validate_session(
    db: &Database,
    token: &str,
    expected_scope: &str,
) -> Result<Option<SessionInfo>> {
    let token_hash = hash_token(token);
    let expected_scope = expected_scope.to_string();
    let now = unix_now() as i64;
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT u.id, u.username, u.display_name, u.role, u.status, u.email_verified, s.scope
             FROM sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND s.scope = ?3
             LIMIT 1",
        )?;
        let mut rows = statement.query(params![token_hash, now, expected_scope])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let status: String = row.get(4)?;
        let role: String = row.get(3)?;
        if status == "disabled" || status == "rejected" || (expected_scope == "admin" && role != "admin") {
            return Ok(None);
        }
        Ok(Some(SessionInfo {
            user_id: row.get(0)?,
            username: row.get(1)?,
            display_name: row.get(2)?,
            role,
            status,
            email_verified: row.get::<_, i64>(5)? != 0,
            scope: row.get(6)?,
        }))
    })
    .await
}

pub async fn delete_session(db: &Database, token: &str) -> Result<()> {
    let token_hash = hash_token(token);
    db.call(move |conn| {
        conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    })
    .await
}

pub async fn revoke_user_sessions(db: &Database, user_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        conn.execute("DELETE FROM sessions WHERE user_id = ?1", [&user_id])?;
        conn.execute(
            "UPDATE client_credentials SET revoked=1 WHERE user_id=?1",
            [&user_id],
        )?;
        Ok(())
    })
    .await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSession {
    pub id: String,
    pub scope: String,
    pub expires_at: i64,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

pub async fn user_sessions(db: &Database, user_id: &str) -> Result<Vec<AccountSession>> {
    let user_id = user_id.to_string();
    let now = unix_now() as i64;
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT token_hash, scope, expires_at, created_at, last_seen_at, user_agent, ip
             FROM sessions WHERE user_id = ?1 AND expires_at > ?2
             ORDER BY created_at DESC LIMIT 100",
        )?;
        let sessions = statement.query_map(params![user_id, now], |row| {
            Ok(AccountSession {
                id: row.get(0)?,
                scope: row.get(1)?,
                expires_at: row.get(2)?,
                created_at: row.get(3)?,
                last_seen_at: row.get(4)?,
                user_agent: row.get(5)?,
                ip: row.get(6)?,
            })
        })?;
        Ok(sessions.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

pub async fn revoke_owned_session(db: &Database, user_id: &str, id: &str) -> Result<bool> {
    let user_id = user_id.to_string();
    let id = id.to_string();
    db.call(move |conn| {
        Ok(conn.execute(
            "DELETE FROM sessions WHERE token_hash = ?1 AND user_id = ?2",
            params![id, user_id],
        )? > 0)
    })
    .await
}

pub async fn issue_email_token(
    db: &Database,
    user_id: Option<&str>,
    email: &str,
    purpose: &str,
    ttl_secs: u64,
) -> Result<String> {
    let token = random_hex(32);
    let token_hash = hash_token(&token);
    let user_id = user_id.map(str::to_string);
    let email = email.trim().to_ascii_lowercase();
    let purpose = purpose.to_string();
    let now = now_iso();
    let expires_at = unix_now() as i64 + ttl_secs as i64;
    db.call(move |conn| {
        conn.execute(
            "DELETE FROM email_tokens WHERE email = ?1 AND purpose = ?2",
            params![email, purpose],
        )?;
        conn.execute(
            "INSERT INTO email_tokens(token_hash, user_id, email, purpose, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![token_hash, user_id, email, purpose, expires_at, now],
        )?;
        Ok(())
    })
    .await?;
    Ok(token)
}

pub struct EmailToken {
    pub user_id: Option<String>,
    pub email: String,
    pub purpose: String,
}

pub async fn consume_email_token(
    db: &Database,
    token: &str,
    purpose: &str,
) -> Result<Option<EmailToken>> {
    let token_hash = hash_token(token);
    let purpose = purpose.to_string();
    let now = unix_now() as i64;
    let now_text = now_iso();
    db.call(move |conn| {
        let transaction = conn.transaction()?;
        let row = transaction
            .query_row(
                "SELECT user_id, email, purpose FROM email_tokens
                 WHERE token_hash = ?1 AND purpose = ?2 AND consumed_at IS NULL AND expires_at > ?3",
                params![token_hash, purpose, now],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((user_id, email, purpose)) = row else {
            return Ok(None);
        };
        transaction.execute(
            "UPDATE email_tokens SET consumed_at = ?2 WHERE token_hash = ?1",
            params![token_hash, now_text],
        )?;
        transaction.commit()?;
        Ok(Some(EmailToken {
            user_id,
            email,
            purpose,
        }))
    })
    .await
}

pub async fn record_submission(
    db: &Database,
    id: &str,
    user_id: &str,
    theme_id: &str,
    name: &str,
    version: i64,
    status: &str,
    auto_published: bool,
) -> Result<()> {
    let id = id.to_string();
    let user_id = user_id.to_string();
    let theme_id = theme_id.to_string();
    let name = name.to_string();
    let status = status.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO authors.submissions
             (id, user_id, theme_id, name, version, status, submitted_at, auto_published)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, auto_published = excluded.auto_published",
            params![
                id,
                user_id,
                theme_id,
                name,
                version,
                status,
                now,
                if auto_published { 1 } else { 0 },
            ],
        )?;
        Ok(())
    })
    .await
}

pub async fn finish_submission(
    db: &Database,
    id: &str,
    status: &str,
    reviewer: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    let id = id.to_string();
    let status = status.to_string();
    let reviewer = reviewer.map(str::to_string);
    let reason = reason.map(str::to_string);
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.submissions
             SET status = ?2, reviewed_at = ?3, reviewed_by = ?4, reject_reason = ?5
             WHERE id = ?1",
            params![id, status, now, reviewer, reason],
        )?;
        Ok(())
    })
    .await
}

pub async fn withdraw_theme_submissions(
    db: &Database,
    user_id: &str,
    theme_id: &str,
) -> Result<()> {
    let user_id = user_id.to_string();
    let theme_id = theme_id.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.submissions
             SET status = 'withdrawn', reviewed_at = ?3, reviewed_by = ?2, reject_reason = NULL
             WHERE user_id = ?1 AND theme_id = ?4 AND status = 'approved'",
            params![user_id, user_id, now, theme_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn restore_theme_submissions(db: &Database, user_id: &str, theme_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let theme_id = theme_id.to_string();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "UPDATE authors.submissions
             SET status = 'approved', reviewed_at = ?3, reviewed_by = ?2, reject_reason = NULL
             WHERE user_id = ?1 AND theme_id = ?4 AND status = 'withdrawn'",
            params![user_id, user_id, now, theme_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete_theme_submissions(db: &Database, user_id: &str, theme_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    let theme_id = theme_id.to_string();
    db.call(move |conn| {
        conn.execute(
            "DELETE FROM authors.submissions WHERE user_id = ?1 AND theme_id = ?2",
            params![user_id, theme_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn submission_owned(db: &Database, id: &str, user_id: &str) -> Result<Option<String>> {
    let id = id.to_string();
    let user_id = user_id.to_string();
    db.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT status FROM authors.submissions WHERE id = ?1 AND user_id = ?2",
                params![id, user_id],
                |row| row.get(0),
            )
            .optional()?)
    })
    .await
}

pub async fn delete_submission(db: &Database, id: &str, user_id: &str) -> Result<()> {
    let id = id.to_string();
    let user_id = user_id.to_string();
    db.call(move |conn| {
        conn.execute(
            "DELETE FROM authors.submissions WHERE id = ?1 AND user_id = ?2",
            params![id, user_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn user_submissions(db: &Database, user_id: &str) -> Result<Vec<serde_json::Value>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let mut statement = conn.prepare(
            "SELECT id, theme_id, name, version, status, submitted_at, reviewed_at, reject_reason, auto_published
             FROM authors.submissions WHERE user_id = ?1 ORDER BY submitted_at DESC LIMIT 200",
        )?;
        let mut rows = statement.query([user_id])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(serde_json::json!({
                "submissionId": row.get::<_, String>(0)?,
                "id": row.get::<_, String>(1)?,
                "name": row.get::<_, Option<String>>(2)?,
                "version": row.get::<_, i64>(3)?,
                "status": row.get::<_, String>(4)?,
                "submittedAt": row.get::<_, String>(5)?,
                "reviewedAt": row.get::<_, Option<String>>(6)?,
                "rejectReason": row.get::<_, Option<String>>(7)?,
                "autoPublished": row.get::<_, i64>(8)? != 0,
            }));
        }
        Ok(items)
    })
    .await
}

pub async fn audit_log(
    db: &Database,
    limit: i64,
    offset: i64,
    actor: Option<&str>,
    action: Option<&str>,
    ip: Option<&str>,
) -> Result<(Vec<serde_json::Value>, i64)> {
    let actor = actor.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string);
    let action = action.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string);
    let ip = ip.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string);
    db.call(move |conn| {
        let mut where_sql = String::from(" WHERE 1 = 1");
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(actor) = &actor {
            where_sql.push_str(" AND (u.username LIKE ? OR a.actor_id = ?)");
            values.push(SqlValue::Text(format!("%{actor}%")));
            values.push(SqlValue::Text(actor.clone()));
        }
        if let Some(action) = &action {
            where_sql.push_str(" AND a.action LIKE ?");
            values.push(SqlValue::Text(format!("%{action}%")));
        }
        if let Some(ip) = &ip {
            where_sql.push_str(" AND a.ip LIKE ?");
            values.push(SqlValue::Text(format!("%{ip}%")));
        }
        let total: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM audit.audit_log a
                 LEFT JOIN users u ON u.id = a.actor_id{where_sql}"
            ),
            rusqlite::params_from_iter(values.iter()),
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(&format!(
            "SELECT a.id, a.actor_id, u.username, a.action, a.target_type, a.target_id,
                    a.detail, a.ip, a.created_at
             FROM audit.audit_log a LEFT JOIN users u ON u.id = a.actor_id{where_sql}
             ORDER BY a.id DESC LIMIT {limit} OFFSET {offset}"
        ))?;
        let mut rows = statement.query(rusqlite::params_from_iter(values.iter()))?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "actorId": row.get::<_, Option<String>>(1)?,
                "actor": row.get::<_, Option<String>>(2)?,
                "action": row.get::<_, String>(3)?,
                "targetType": row.get::<_, Option<String>>(4)?,
                "targetId": row.get::<_, Option<String>>(5)?,
                "detail": row.get::<_, Option<String>>(6)?,
                "ip": row.get::<_, Option<String>>(7)?,
                "createdAt": row.get::<_, String>(8)?,
            }));
        }
        Ok((items, total))
    })
    .await
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn configured_admin_from_env(paths: &Paths) -> bool {
    let hash = read_env(&paths.admin_env)
        .get("ADMIN_PASSWORD_HASH")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    hash || Path::new(&paths.admin_password_file).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_user_and_validates_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).await.unwrap();
        let user = create_user(
            &db,
            NewUser {
                username: "tester",
                display_name: "Tester",
                email: Some("tester@example.com"),
                password_hash: Some("hash"),
                role: "user",
                status: "active",
                email_verified: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(user.username, "tester");
        assert_eq!(user.status, "active");
        let token = create_session(&db, &user.id, "user", 60, "test", "local")
            .await
            .unwrap();
        let session = validate_session(&db, &token, "user")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.user_id, user.id);
        assert!(validate_session(&db, &token, "admin")
            .await
            .unwrap()
            .is_none());
        revoke_user_sessions(&db, &user.id).await.unwrap();
        assert!(validate_session(&db, &token, "user")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn email_token_is_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).await.unwrap();
        let token = issue_email_token(&db, None, "a@example.com", "verify_email", 60)
            .await
            .unwrap();
        assert!(consume_email_token(&db, &token, "verify_email")
            .await
            .unwrap()
            .is_some());
        assert!(consume_email_token(&db, &token, "verify_email")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn audit_log_filters_by_actor_action_and_ip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_test(dir.path()).await.unwrap();
        db.call(|conn| {
            conn.execute(
                "INSERT INTO users(id, username, display_name, status, created_at, updated_at)
                 VALUES ('u1', 'alice', 'Alice', 'active', '2026-01-01T00:00:00Z',
                         '2026-01-01T00:00:00Z')",
                [],
            )?;
            conn.execute(
                "INSERT INTO audit.audit_log(actor_id, action, target_type, target_id, detail, ip, created_at)
                 VALUES ('u1', 'user.approve', 'user', 'u2', 'ok', '1.2.3.4', '2026-01-01T00:00:00Z')",
                [],
            )?;
            conn.execute(
                "INSERT INTO audit.audit_log(actor_id, action, target_type, target_id, detail, ip, created_at)
                 VALUES ('u1', 'theme.approve', 'theme', 't1', 'ok', '5.6.7.8', '2026-01-01T00:00:01Z')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let (items, total) = audit_log(
            &db,
            10,
            0,
            Some("alice"),
            Some("user.approve"),
            Some("1.2.3.4"),
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0]["action"], "user.approve");
        assert_eq!(items[0]["ip"], "1.2.3.4");
        let (_, total) = audit_log(&db, 10, 0, None, None, Some("5.6")).await.unwrap();
        assert_eq!(total, 1);
    }
}
