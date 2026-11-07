//! Per-installation read credentials. Access and rotating refresh secrets are
//! stored hashed; neither is a site-wide password or an upload/admin credential.
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::{hash_token, SessionInfo};
use crate::api::{ApiError, AppState};
use crate::auth;
use crate::db::Database;
use crate::util::{random_hex, unix_now};

pub const ACCESS_SECONDS: i64 = 900;
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS client_credentials (
 id TEXT PRIMARY KEY, user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
 name TEXT NOT NULL, access_hash TEXT NOT NULL UNIQUE,
 refresh_hash TEXT NOT NULL UNIQUE, previous_refresh_hash TEXT,
 prefix TEXT NOT NULL, access_expires INTEGER NOT NULL,
 expires_at INTEGER NOT NULL, created_at INTEGER NOT NULL,
 last_used_at INTEGER, revoked INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS client_credentials_owner ON client_credentials(user_id, created_at);
CREATE INDEX IF NOT EXISTS client_credentials_previous ON client_credentials(previous_refresh_hash);
"#;

pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/client/access", post(bootstrap))
        .route("/client/refresh", post(refresh))
        .layer(axum::extract::DefaultBodyLimit::max(16384))
}

pub fn account_router() -> Router<AppState> {
    Router::new()
        .route("/access", get(my_access).post(create_personal))
        .route("/access/revoke", post(revoke_personal))
}

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/client-credentials", get(all_access))
        .route("/client-credentials/revoke", post(revoke_admin))
}

fn secret(prefix: &str) -> String {
    format!("{prefix}{}", random_hex(32))
}
fn valid_secret(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn now() -> i64 {
    unix_now() as i64
}

fn no_store(value: Value) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert("Cache-Control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("Referrer-Policy", "no-referrer".parse().unwrap());
    response
}

fn request_guard(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers
        .get(crate::config::CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        != Some(crate::config::CSRF_VALUE)
    {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "缺少请求校验头"));
    }
    Ok(())
}

async fn actor(state: &AppState, headers: &HeaderMap) -> Result<Option<SessionInfo>, ApiError> {
    let bearer = auth::bearer_token(headers);
    let user = auth::session_token(headers, auth::USER_SESSION_COOKIE);
    let admin = auth::session_token(headers, auth::ADMIN_SESSION_COOKIE);
    for (token, scope) in [(&bearer, "user"), (&user, "user"), (&admin, "admin")] {
        if let Some(token) = token {
            if let Some(info) = state.sessions.validate(token, scope).await {
                return Ok(Some(info));
            }
        }
    }
    if bearer.is_some() || user.is_some() || admin.is_some() {
        return Err(ApiError::unauthorized("登录已过期，请重新登录"));
    }
    Ok(None)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Create {
    #[serde(default)]
    name: String,
    #[serde(default)]
    expires_days: Option<i64>,
}

fn name(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.chars().count() > 64 || value.chars().any(char::is_control) {
        return Err(ApiError::bad("名称最多 64 个字符"));
    }
    Ok(if value.is_empty() {
        "管理器设备".into()
    } else {
        value.into()
    })
}

pub async fn issue(
    db: &Database,
    user_id: Option<String>,
    name: String,
    days: i64,
) -> anyhow::Result<Value> {
    let access = secret("sa_");
    let refresh = secret("sr_");
    let id = format!("device-{}", random_hex(12));
    let at = now();
    let expires = at + days * 86400;
    let out = json!({"ok": true, "id": id, "accessToken": access, "refreshToken": refresh,
        "expiresAt": at + ACCESS_SECONDS, "refreshExpiresAt": expires, "refreshAfter": at + ACCESS_SECONDS - 120,
        "tokenType": "X-Sukisu-Theme-Token", "scopes": ["read"], "userId": user_id});
    let access_hash = hash_token(&access);
    let refresh_hash = hash_token(&refresh);
    let prefix = access[..11].to_string();
    db.call(move |conn| {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM client_credentials WHERE expires_at < ?1 - 86400", [at])?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM client_credentials WHERE user_id IS ?1 AND revoked = 0 AND expires_at > ?2", params![user_id, at], |r| r.get(0))?;
        // Anonymous installs are separately IP-limited at the public boundary.
        if count >= if user_id.is_some() { 30 } else { 100000 } {
            return Err(tokio_rusqlite::Error::Other(anyhow::anyhow!("有效设备数量已达上限，请先撤销旧设备").into()));
        }
        tx.execute("INSERT INTO client_credentials(id,user_id,name,access_hash,refresh_hash,prefix,access_expires,expires_at,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id,user_id,name,access_hash,refresh_hash,prefix,at+ACCESS_SECONDS,expires,at])?;
        tx.commit()?;
        Ok(())
    }).await?;
    Ok(out)
}

async fn bootstrap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Create>,
) -> Result<axum::response::Response, ApiError> {
    request_guard(&headers)?;
    let ip = crate::api::request_ip(&headers);
    if !crate::account_api::rate_allowed(&state, &format!("client-issue:{ip}"), 60, 3600) {
        return Err(ApiError::too_many("设备凭证申请过于频繁"));
    }
    let user = actor(&state, &headers).await?;
    if let Some(user) = &user {
        crate::account_api::require_active(user)?;
    }
    if user.is_none()
        && crate::config::read_env(&state.paths.admin_config)
            .get("CLIENT_GUEST_ACCESS")
            .is_some_and(|v| v == "0")
    {
        return Err(ApiError::unauthorized("本站要求先登录再获取设备凭证"));
    }
    let mut result = issue(&state.db, user.map(|u| u.user_id), name(&body.name)?, 90)
        .await
        .map_err(ApiError::internal)?;
    result["endpoints"] = crate::site::public_endpoints(&state.paths);
    Ok(no_store(result))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Refresh {
    refresh_token: String,
}

pub async fn rotate(db: &Database, provided: &str) -> anyhow::Result<Option<Value>> {
    if !valid_secret(provided, "sr_") {
        return Ok(None);
    }
    let hash = hash_token(provided);
    let access = secret("sa_");
    let refresh = secret("sr_");
    let new_hash = hash_token(&refresh);
    let access_hash = hash_token(&access);
    let prefix = access[..11].to_string();
    let at = now();
    db.call(move |conn| {
        let tx = conn.transaction()?;
        // A replay of the immediately previous refresh secret revokes the family.
        let replay: Option<String> = tx.query_row("SELECT id FROM client_credentials WHERE previous_refresh_hash=?1 AND revoked=0", [&hash], |r| r.get(0)).optional()?;
        if let Some(id) = replay {
            tx.execute("UPDATE client_credentials SET revoked=1 WHERE id=?1", [id])?;
            tx.commit()?;
            return Ok(None);
        }
        let row: Option<(String, Option<String>, i64)> = tx.query_row(
            "SELECT c.id,c.user_id,c.expires_at FROM client_credentials c LEFT JOIN users u ON c.user_id=u.id WHERE c.refresh_hash=?1 AND c.revoked=0 AND c.expires_at>?2 AND (c.user_id IS NULL OR u.status='active')",
            params![hash,at], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((id, user_id, expires)) = row else { return Ok(None); };
        let access_expires = (at + ACCESS_SECONDS).min(expires);
        tx.execute("UPDATE client_credentials SET previous_refresh_hash=refresh_hash,refresh_hash=?1,access_hash=?2,prefix=?3,access_expires=?4,last_used_at=?5 WHERE id=?6", params![new_hash,access_hash,prefix,access_expires,at,id])?;
        tx.commit()?;
        Ok(Some(json!({"ok":true,"id":id,"accessToken":access,"refreshToken":refresh,"expiresAt":access_expires,"refreshExpiresAt":expires,"refreshAfter":access_expires-120,"userId":user_id,"scopes":["read"]})))
    }).await
}

async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Refresh>,
) -> Result<axum::response::Response, ApiError> {
    request_guard(&headers)?;
    if !crate::account_api::rate_allowed(
        &state,
        &format!("client-refresh:{}", crate::api::request_ip(&headers)),
        120,
        60,
    ) {
        return Err(ApiError::too_many("刷新过于频繁"));
    }
    let mut result = rotate(&state.db, &body.refresh_token)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("设备凭证已过期、撤销或重复使用，请重新授权"))?;
    state.invalidate_token_cache();
    result["endpoints"] = crate::site::public_endpoints(&state.paths);
    Ok(no_store(result))
}

pub async fn validate(db: &Database, provided: &str) -> bool {
    if !valid_secret(provided, "sa_") {
        return false;
    }
    let hash = hash_token(provided);
    let at = now();
    db.call(move |conn| {
        let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM client_credentials c LEFT JOIN users u ON c.user_id=u.id WHERE c.access_hash=?1 AND c.revoked=0 AND c.access_expires>?2 AND c.expires_at>?2 AND (c.user_id IS NULL OR u.status='active'))", params![hash,at], |r| r.get(0))?;
        if valid { conn.execute("UPDATE client_credentials SET last_used_at=?2 WHERE access_hash=?1 AND (last_used_at IS NULL OR last_used_at < ?2-60)", params![hash,at])?; }
        Ok(valid)
    }).await.unwrap_or(false)
}

async fn list(db: &Database, owner: Option<String>) -> anyhow::Result<Value> {
    db.call(move |conn| {
        let mut stmt = conn.prepare("SELECT c.id,c.user_id,c.name,c.prefix,c.created_at,c.expires_at,c.access_expires,c.last_used_at,c.revoked,u.username FROM client_credentials c LEFT JOIN users u ON c.user_id=u.id WHERE (?1 IS NULL OR c.user_id=?1) ORDER BY c.created_at DESC LIMIT 500")?;
        let rows = stmt.query_map([owner], |r| Ok(json!({"id":r.get::<_,String>(0)?,"userId":r.get::<_,Option<String>>(1)?,"name":r.get::<_,String>(2)?,"prefix":r.get::<_,String>(3)?,"createdAt":r.get::<_,i64>(4)?,"expiresAt":r.get::<_,i64>(5)?,"accessExpiresAt":r.get::<_,i64>(6)?,"lastUsedAt":r.get::<_,Option<i64>>(7)?,"revoked":r.get::<_,bool>(8)?,"username":r.get::<_,Option<String>>(9)?})))?;
        Ok(json!({"ok":true,"credentials":rows.collect::<rusqlite::Result<Vec<_>>>()?}))
    }).await
}

async fn my_access(
    State(state): State<AppState>,
    Extension(user): Extension<SessionInfo>,
) -> Result<axum::response::Response, ApiError> {
    let mut data = list(&state.db, Some(user.user_id))
        .await
        .map_err(ApiError::internal)?;
    data["endpoints"] = crate::site::public_endpoints(&state.paths);
    Ok(no_store(data))
}

async fn create_personal(
    State(state): State<AppState>,
    Extension(user): Extension<SessionInfo>,
    Json(body): Json<Create>,
) -> Result<axum::response::Response, ApiError> {
    crate::account_api::require_active(&user)?;
    let days = body.expires_days.unwrap_or(90);
    if !(1..=365).contains(&days) {
        return Err(ApiError::bad("有效期需为 1–365 天"));
    }
    let mut result = issue(&state.db, Some(user.user_id), name(&body.name)?, days)
        .await
        .map_err(ApiError::internal)?;
    result["endpoints"] = crate::site::public_endpoints(&state.paths);
    Ok(no_store(result))
}

#[derive(Deserialize)]
struct Revoke {
    id: String,
}
async fn revoke(
    state: &AppState,
    id: String,
    owner: Option<String>,
) -> Result<axum::response::Response, ApiError> {
    let changed = state.db.call(move |conn| Ok(conn.execute("UPDATE client_credentials SET revoked=1 WHERE id=?1 AND (?2 IS NULL OR user_id=?2)", params![id,owner])?)).await.map_err(ApiError::internal)?;
    if changed == 0 {
        return Err(ApiError::not_found("设备不存在"));
    }
    state.invalidate_token_cache();
    Ok(no_store(json!({"ok":true})))
}
async fn revoke_personal(
    State(state): State<AppState>,
    Extension(user): Extension<SessionInfo>,
    Json(body): Json<Revoke>,
) -> Result<axum::response::Response, ApiError> {
    revoke(&state, body.id, Some(user.user_id)).await
}
async fn revoke_admin(
    State(state): State<AppState>,
    Json(body): Json<Revoke>,
) -> Result<axum::response::Response, ApiError> {
    revoke(&state, body.id, None).await
}
async fn all_access(State(state): State<AppState>) -> Result<axum::response::Response, ApiError> {
    Ok(no_store(
        list(&state.db, None).await.map_err(ApiError::internal)?,
    ))
}
