use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

use crate::accounts::{self, SessionInfo};
use crate::config::{read_env, Paths};
use crate::db::Database;

pub const ADMIN_SESSION_COOKIE: &str = "sukisu_admin_session";
pub const USER_SESSION_COOKIE: &str = "sukisu_user_session";
pub const SESSION_TTL_SECS: u64 = 12 * 60 * 60;
pub const USER_SESSION_TTL_SECS: u64 = 12 * 60 * 60;

#[derive(Clone)]
pub struct SessionStore {
    db: Database,
}

impl SessionStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn create(
        &self,
        user_id: &str,
        scope: &str,
        ttl_secs: u64,
        user_agent: &str,
        ip: &str,
    ) -> anyhow::Result<String> {
        accounts::create_session(&self.db, user_id, scope, ttl_secs, user_agent, ip).await
    }

    pub async fn validate(&self, token: &str, scope: &str) -> Option<SessionInfo> {
        match accounts::validate_session(&self.db, token, scope).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("session validation failed: {error:#}");
                None
            }
        }
    }

    pub async fn remove(&self, token: &str) {
        if let Err(error) = accounts::delete_session(&self.db, token).await {
            tracing::warn!("session removal failed: {error:#}");
        }
    }

    pub async fn revoke_user(&self, user_id: &str) {
        if let Err(error) = accounts::revoke_user_sessions(&self.db, user_id).await {
            tracing::warn!("session revocation failed: {error:#}");
        }
    }
}

pub fn admin_username() -> String {
    std::env::var("ADMIN_USERNAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "admin".to_string())
}

pub fn configured_username(paths: &Paths) -> String {
    read_env(&paths.admin_env)
        .get("ADMIN_USERNAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(admin_username)
}

pub async fn admin_configured(db: &Database, paths: &Paths) -> bool {
    if let Ok(count) = accounts::admin_count(db).await {
        if count > 0 {
            return true;
        }
    }
    let env = read_env(&paths.admin_env);
    if env
        .get("ADMIN_PASSWORD_HASH")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    std::fs::read_to_string(&paths.admin_password_file)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

pub fn hash_admin_password(password: &str) -> Result<String, String> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|error| error.to_string())
}

pub fn valid_admin_username(value: &str) -> bool {
    let value = value.trim();
    (3..=32).contains(&value.len())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

pub fn verify_admin_password(
    paths: &Paths,
    username: &str,
    password: &str,
) -> Result<bool, String> {
    if username != configured_username(paths) {
        return Ok(false);
    }
    let env = read_env(&paths.admin_env);
    let hash = env
        .get("ADMIN_PASSWORD_HASH")
        .map(|value| value.trim())
        .unwrap_or("");
    if !hash.is_empty() {
        return bcrypt::verify(password, hash).map_err(|error| error.to_string());
    }
    if let Ok(text) = std::fs::read_to_string(&paths.admin_password_file) {
        let plain = text.trim_end_matches(&['\r', '\n'][..]);
        let expected = plain.as_bytes();
        let provided = password.as_bytes();
        if expected.len() == provided.len() {
            let equal: bool = expected.ct_eq(provided).into();
            return Ok(equal);
        }
    }
    Ok(false)
}

pub fn session_token(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if name == cookie_name {
            return Some(value.to_string());
        }
    }
    None
}

pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("authorization")?.to_str().ok()?.trim();
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

pub fn session_cookie(name: &str, token: &str, path: &str, ttl_secs: u64, secure: bool) -> String {
    format!(
        "{name}={token}; Path={path}; HttpOnly; SameSite=Strict; Max-Age={ttl_secs}{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_session_cookie(name: &str, path: &str, secure: bool) -> String {
    format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}
