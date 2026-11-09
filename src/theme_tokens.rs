//! Named, revocable theme access tokens (multi-token support).
//!
//! There is no shared master fallback; tokens created here are stored as SHA-256 hashes (same
//! scheme as web sessions) and only their prefix is ever shown again after
//! the one-time reveal on creation.

use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{SecondsFormat, TimeZone, Utc};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde::de::{Deserializer, Visitor};
use serde_json::{json, Value};
use std::marker::PhantomData;

use crate::accounts::{audit, hash_token, SessionInfo};
use crate::api::{json_body, ApiError, AppState};
use crate::db::Database;
use crate::util::{now_iso, random_hex, unix_now};

type ApiResult<T> = Result<T, ApiError>;

pub const SCOPE_READ: &str = "read";
pub const SCOPE_UPLOAD: &str = "upload";
const SCOPES: [&str; 2] = [SCOPE_READ, SCOPE_UPLOAD];

/// `last_used_at` is rewritten at most once per this many seconds per token
/// so hot download paths never pay for a redundant write.
const LAST_USED_WRITE_INTERVAL_SECS: i64 = 60;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tokens", get(list_tokens))
        .route("/tokens/create", post(create_token))
        .route("/tokens/update", post(update_token))
        .route("/tokens/delete", post(delete_token))
}

fn scope_allowed(stored: &str, scope: &str) -> bool {
    stored.split_whitespace().any(|value| value == scope)
}

/// Normalizes user-supplied scopes into canonical storage order. Returns
/// `None` when the selection is empty or contains an unknown scope.
fn normalize_scopes(values: &[String]) -> Option<String> {
    let mut picked: Vec<&str> = Vec::new();
    for value in values {
        let value = value.trim().to_ascii_lowercase();
        let canonical = SCOPES.iter().find(|scope| **scope == value)?;
        if !picked.contains(canonical) {
            picked.push(canonical);
        }
    }
    if picked.is_empty() {
        return None;
    }
    picked.sort_unstable();
    Some(picked.join(" "))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 64 && !name.chars().any(char::is_control)
}

/// ISO string for a unix second; same format as `now_iso` so plain string
/// comparison stays chronological.
fn iso_at_unix(unix: i64) -> String {
    Utc.timestamp_opt(unix, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[derive(Debug, Clone)]
pub struct TokenRecord {
    id: String,
    name: String,
    token_prefix: String,
    scopes: String,
    enabled: bool,
    created_at: String,
    expires_at: Option<i64>,
    last_used_at: Option<String>,
}

impl TokenRecord {
    fn is_live(&self) -> bool {
        self.enabled && self.expires_at.is_none_or(|at| at > unix_now() as i64)
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "prefix": self.token_prefix,
            "scopes": self.scopes.split_whitespace().collect::<Vec<_>>(),
            "enabled": self.enabled,
            "createdAt": self.created_at,
            "expiresAt": self.expires_at,
            "expired": self.expires_at.is_some_and(|at| at <= unix_now() as i64),
            "lastUsedAt": self.last_used_at,
        })
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRecord> {
    Ok(TokenRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        token_prefix: row.get(2)?,
        scopes: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        last_used_at: row.get(7)?,
    })
}

const ROW_COLUMNS: &str = "id, name, token_prefix, scopes, enabled, created_at, expires_at, last_used_at";

/// Returns the plaintext token alongside the row id; the plaintext is never
/// stored or recoverable afterwards.
pub async fn create_token_row(
    db: &Database,
    name: &str,
    scopes: &str,
    expires_days: Option<i64>,
) -> anyhow::Result<(String, String)> {
    let id = format!("tok-{}", random_hex(8));
    let token = random_hex(32);
    let token_hash = hash_token(&token);
    let token_prefix = token[..8].to_string();
    let name = name.to_string();
    let scopes = scopes.to_string();
    let created_at = now_iso();
    let expires_at = expires_days.map(|days| unix_now() as i64 + days * 86_400);
    let id_for_insert = id.clone();
    db.call_write(move |conn| {
        conn.execute(
            "INSERT INTO theme_tokens(id, name, token_hash, token_prefix, scopes, enabled, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
            params![id_for_insert, name, token_hash, token_prefix, scopes, created_at, expires_at],
        )?;
        Ok(())
    })
    .await?;
    Ok((id, token))
}

pub async fn list_token_records(db: &Database) -> anyhow::Result<Vec<TokenRecord>> {
    db.call(|conn| {
        let mut statement =
            conn.prepare(&format!("SELECT {ROW_COLUMNS} FROM theme_tokens ORDER BY created_at DESC"))?;
        let rows = statement.query_map([], row_to_record)?;
        let mut values = Vec::new();
        for row in rows {
            values.push(row?);
        }
        Ok(values)
    })
    .await
}

/// Applies the editable subset of a token. `expires_days` keeps the serde
/// tri-state via `set_expiry`: absent = keep current, else the (nullable)
/// value is written — `None` inside `Some` means "never expires".
pub async fn update_token_row(
    db: &Database,
    id: &str,
    name: Option<&str>,
    scopes: Option<&str>,
    enabled: Option<bool>,
    set_expiry: Option<Option<i64>>,
) -> anyhow::Result<bool> {
    if name.is_none() && scopes.is_none() && enabled.is_none() && set_expiry.is_none() {
        return Ok(false);
    }
    let id = id.to_string();
    let name = name.map(str::to_string);
    let scopes = scopes.map(str::to_string);
    let enabled = enabled.map(|value| value as i64);
    let expires_at =
        set_expiry.map(|days| days.map(|days| unix_now() as i64 + days * 86_400));
    let set_expiry = set_expiry.is_some() as i64;
    db.call_write(move |conn| {
        let changed = conn.execute(
            "UPDATE theme_tokens
             SET name = COALESCE(?1, name),
                 scopes = COALESCE(?2, scopes),
                 enabled = COALESCE(?3, enabled),
                 expires_at = CASE WHEN ?4 = 1 THEN ?5 ELSE expires_at END
             WHERE id = ?6",
            params![name, scopes, enabled, set_expiry, expires_at.flatten(), id],
        )?;
        Ok(changed > 0)
    })
    .await
}

pub async fn delete_token_row(db: &Database, id: &str) -> anyhow::Result<bool> {
    let id = id.to_string();
    db.call_write(move |conn| {
        let changed = conn.execute("DELETE FROM theme_tokens WHERE id = ?1", [id])?;
        Ok(changed > 0)
    })
    .await
}

pub async fn get_token(db: &Database, id: &str) -> anyhow::Result<Option<TokenRecord>> {
    let id = id.to_string();
    db.call(move |conn| {
        let record = conn
            .query_row(
                &format!("SELECT {ROW_COLUMNS} FROM theme_tokens WHERE id = ?1"),
                [id],
                row_to_record,
            )
            .optional()?;
        Ok(record)
    })
    .await
}

/// True when a stored token exists that is enabled, unexpired and carries
/// `scope`. Matching happens on the SHA-256 hash; guessing the plaintext is
/// as hard as guessing a web session id.
pub async fn validate(db: &Database, token: &str, scope: &str) -> bool {
    let token_hash = hash_token(token);
    let scope = scope.to_string();
    let now = unix_now() as i64;
    let cutoff_iso = iso_at_unix(now - LAST_USED_WRITE_INTERVAL_SECS);
    let now_iso_value = now_iso();
    let result = db
        .call(move |conn| {
            let scopes: Option<String> = conn
                .query_row(
                    "SELECT scopes FROM theme_tokens
                     WHERE token_hash = ?1 AND enabled = 1
                       AND (expires_at IS NULL OR expires_at > ?2)",
                    params![token_hash, now],
                    |row| row.get(0),
                )
                .optional()?;
            let matched = scopes
                .map(|scopes| scope_allowed(&scopes, &scope))
                .unwrap_or(false);
            if matched {
                conn.execute(
                    "UPDATE theme_tokens SET last_used_at = ?2
                     WHERE token_hash = ?1
                       AND (last_used_at IS NULL OR last_used_at < ?3)",
                    params![token_hash, now_iso_value, cutoff_iso],
                )?;
            }
            Ok(matched)
        })
        .await;
    result.unwrap_or(false)
}

/// Resolve a named token to its display name for the API monitor. Returns
/// `None` for disabled/expired/unknown tokens; callers still label the request
/// as a theme-token request so invalid attempts remain visible.
pub async fn name_for_token(db: &Database, token: &str) -> Option<String> {
    let token_hash = hash_token(token);
    let now = unix_now() as i64;
    db.call(move |conn| {
        let name: Option<String> = conn
            .query_row(
                "SELECT name FROM theme_tokens
                 WHERE token_hash = ?1 AND enabled = 1
                   AND (expires_at IS NULL OR expires_at > ?2)
                 LIMIT 1",
                params![token_hash, now],
                |row| row.get(0),
            )
            .optional()?;
        Ok(name)
    })
    .await
    .ok()
    .flatten()
}

/// Enabled, unexpired named script token count for the admin status panel.
pub async fn enabled_count(db: &Database) -> anyhow::Result<i64> {
    let now = unix_now() as i64;
    db.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM theme_tokens
             WHERE enabled = 1 AND (expires_at IS NULL OR expires_at > ?1)",
            [now],
            |row| row.get(0),
        )?;
        Ok(count)
    })
    .await
}

async fn list_tokens(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let tokens = list_token_records(&state.db)
        .await
        .map_err(ApiError::internal)?;
    let enabled = tokens.iter().filter(|token| token.is_live()).count();
    Ok(Json(json!({
        "ok": true,
        "tokens": tokens.iter().map(TokenRecord::to_json).collect::<Vec<_>>(),
        "count": tokens.len(),
        "enabled": enabled,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    name: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expires_days: Option<i64>,
}

fn validate_expiry(days: Option<i64>) -> ApiResult<Option<i64>> {
    match days {
        None => Ok(None),
        Some(days @ 1..=3650) => Ok(Some(days)),
        Some(_) => Err(ApiError::bad("有效期天数需在 1-3650 之间")),
    }
}

async fn create_token(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let input: CreateBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("令牌参数格式错误：{error}")))?;
    let name = input.name.trim();
    if !valid_name(name) {
        return Err(ApiError::bad("令牌名称需为 1-64 个字符"));
    }
    let scopes =
        normalize_scopes(&input.scopes).ok_or_else(|| ApiError::bad("权限只能是 read、upload 的任意组合"))?;
    let expiry = validate_expiry(input.expires_days)?;
    let (id, token) = create_token_row(&state.db, name, &scopes, expiry)
        .await
        .map_err(|error| ApiError::bad(error.to_string()))?;
    state.invalidate_token_cache();
    let _ = audit(
        &state.db,
        Some(&actor.user_id),
        "theme.token.create",
        Some("theme_token"),
        Some(&id),
        Some(name),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "id": id,
        "token": token,
        "message": "令牌已创建，离开弹窗后将无法再次查看",
    })))
}

// Plain `Option<Option<T>>` cannot tell `"expiresDays": null` from an
// omitted field (both deserialize to `None`), so a custom visitor maps a
// present null to `Some(None)` (clear the expiry) while a missing field
// keeps the serde `default` of `None` (do not touch it).
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    de.deserialize_option(PresentThenInner(PhantomData))
}

struct PresentThenInner<T>(PhantomData<T>);

impl<'de, T> Visitor<'de> for PresentThenInner<T>
where
    T: Deserialize<'de>,
{
    type Value = Option<Option<T>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a number or null")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Some(None))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Some(None))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(|value| Some(Some(value)))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    enabled: Option<bool>,
    // Outer Option = field present; inner Option = expiry value vs cleared.
    #[serde(default, deserialize_with = "double_option")]
    expires_days: Option<Option<i64>>,
}

async fn update_token(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let input: UpdateBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("令牌参数格式错误：{error}")))?;
    if get_token(&state.db, &input.id)
        .await
        .map_err(ApiError::internal)?
        .is_none()
    {
        return Err(ApiError::not_found("令牌不存在"));
    }
    let name = match input.name.as_deref() {
        Some(name) => {
            let name = name.trim();
            if !valid_name(name) {
                return Err(ApiError::bad("令牌名称需为 1-64 个字符"));
            }
            Some(name)
        }
        None => None,
    };
    let scopes = match input.scopes.as_deref() {
        Some(scopes) => Some(
            normalize_scopes(scopes)
                .ok_or_else(|| ApiError::bad("权限只能是 read、upload 的任意组合"))?,
        ),
        None => None,
    };
    let expiry = validate_expiry(input.expires_days.flatten())?;
    let set_expiry = input.expires_days.map(|_| expiry);
    let changed = update_token_row(
        &state.db,
        &input.id,
        name,
        scopes.as_deref(),
        input.enabled,
        set_expiry,
    )
    .await
    .map_err(ApiError::internal)?;
    let detail = json!({
        "name": input.name,
        "scopes": input.scopes,
        "enabled": input.enabled,
        "expiresDays": input.expires_days,
    })
    .to_string();
    let _ = audit(
        &state.db,
        Some(&actor.user_id),
        "theme.token.update",
        Some("theme_token"),
        Some(&input.id),
        Some(&detail),
    )
    .await;
    if !changed {
        return Err(ApiError::bad("没有需要更新的字段"));
    }
    state.invalidate_token_cache();
    Ok(Json(json!({"ok": true, "message": "令牌已更新"})))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteBody {
    id: String,
}

async fn delete_token(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let input: DeleteBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("令牌参数格式错误：{error}")))?;
    let Some(existing) = get_token(&state.db, &input.id)
        .await
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::not_found("令牌不存在"));
    };
    let changed = delete_token_row(&state.db, &input.id)
        .await
        .map_err(ApiError::internal)?;
    let _ = audit(
        &state.db,
        Some(&actor.user_id),
        "theme.token.delete",
        Some("theme_token"),
        Some(&input.id),
        Some(&existing.name),
    )
    .await;
    if !changed {
        return Err(ApiError::not_found("令牌不存在"));
    }
    state.invalidate_token_cache();
    Ok(Json(json!({"ok": true, "message": "令牌已删除"})))
}
