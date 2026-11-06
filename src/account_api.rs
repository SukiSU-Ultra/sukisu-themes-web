use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use rand::seq::SliceRandom;
use rand::Rng;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::{self, SessionInfo, User};
use crate::api::{header_text, json_body, request_ip, ApiError, AppState};
use crate::auth;
use crate::authors;
use crate::config::AVATAR_MAX_EDGE;
use crate::follows;
use crate::images;
use crate::mailer;
use crate::oauth;
use crate::util::{
    author_fingerprint, author_key, content_type_for, now_iso, quote_query, random_hex, sha256_hex,
    unix_now, valid_author_hash, valid_author_id, valid_theme_id,
};

type ApiResult<T> = Result<T, ApiError>;

const CSRF_HEADER: &str = "X-Requested-With";
const CSRF_VALUE: &str = "SukiSUThemeAdmin";
const PUBLIC_PATHS: &[&str] = &[
    "/config",
    "/register",
    "/auth/login",
    "/auth/logout",
    "/session",
    "/auth/verify-email",
    "/auth/resend-verification",
    "/auth/forgot-password",
    "/auth/reset-password",
    "/auth/github/start",
    "/auth/github/callback",
    "/auth/device/start",
    "/auth/device/poll",
    "/auth/device/info",
];

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/config", get(config))
        .route("/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/session", get(session_info))
        .route("/auth/verify-email", get(verify_email))
        .route("/auth/resend-verification", post(resend_verification))
        .route("/auth/forgot-password", post(forgot_password))
        .route("/auth/reset-password", post(reset_password))
        .route("/auth/github/start", get(github_start))
        .route("/auth/github/callback", get(github_callback))
        .route("/auth/github/unlink", post(github_unlink))
        .route("/auth/device/start", post(device_start))
        .route("/auth/device/poll", get(device_poll))
        .route("/auth/device/info", get(device_info))
        .route("/auth/device/approve", post(device_approve))
        .route("/follows", get(my_follows))
        .route("/follows/follow", post(follow_author))
        .route("/follows/unfollow", post(unfollow_author))
        .route("/follows/leaderboard", get(author_leaderboard))
        .route(
            "/preferences",
            get(get_preferences).post(update_preferences),
        )
        .route("/profile", get(profile).post(update_profile))
        .route("/sessions", get(list_sessions))
        .route("/sessions/revoke", post(revoke_session))
        .route("/password", post(change_password))
        .route("/email", post(change_email))
        .route("/avatar", post(upload_avatar))
        .route("/authors", get(my_authors))
        .route(
            "/author/requests",
            get(my_author_requests).post(create_author_request),
        )
        .route(
            "/author/requests/{id}/generate",
            post(generate_reset_author),
        )
        .route("/discover", get(discover_themes))
        .route(
            "/themes/{id}/comments",
            get(crate::comments::account_list).post(crate::comments::account_create),
        )
        .route("/comments/delete", post(crate::comments::account_delete))
        .route("/comments/clear", post(crate::comments::account_clear))
        .route("/messages", get(crate::comments::account_messages))
        .route("/users/search", get(crate::comments::account_user_search))
        .route("/users/{id}/avatar", get(crate::comments::user_avatar))
        .route("/themes/{id}/download", get(download_published_theme))
        .route("/my-themes", get(my_themes))
        .route("/my-themes/{id}/download", get(download_my_theme))
        .route("/my-themes/withdraw", post(withdraw_my_theme))
        .route("/my-themes/restore", post(restore_my_theme))
        .route("/my-themes/delete", post(delete_my_theme))
        .route("/submissions", get(my_submissions))
        .route("/submissions/withdraw", post(withdraw_submission))
        .route("/submissions/delete", post(delete_submission))
        .route("/preview", get(account_preview))
        .route("/theme-id/next", get(next_theme_id))
        .route("/submit", post(submit_theme))
        .merge(crate::community::account_router())
        .merge(crate::resumable::router())
        .merge(crate::client_access::account_router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            account_csrf_guard,
        ))
        .layer(middleware::from_fn_with_state(state, account_guard))
}

async fn account_guard(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let path = request
        .uri()
        .path()
        .strip_prefix("/account/api")
        .unwrap_or(request.uri().path())
        .to_string();
    if PUBLIC_PATHS.contains(&path.as_str()) {
        return next.run(request).await;
    }
    // Avatars are public profile images.
    if request.method() == axum::http::Method::GET
        && path.starts_with("/users/")
        && path.ends_with("/avatar")
    {
        return next.run(request).await;
    }
    let headers = request.headers();
    let bearer = auth::bearer_token(headers);
    let user_cookie = auth::session_token(headers, auth::USER_SESSION_COOKIE);
    let admin_cookie = auth::session_token(headers, auth::ADMIN_SESSION_COOKIE);
    let has_credentials = bearer.is_some() || user_cookie.is_some() || admin_cookie.is_some();
    let session = if let Some(token) = bearer.as_deref().or(user_cookie.as_deref()) {
        state.sessions.validate(token, "user").await
    } else {
        None
    };
    let session = if session.is_some() {
        session
    } else if let Some(token) = admin_cookie.as_deref() {
        state.sessions.validate(token, "admin").await
    } else {
        None
    };
    let Some(session) = session else {
        // A supplied but invalid credential must fail loudly instead of
        // silently downgrading the caller to a guest; otherwise a logged-in
        // client keeps an expired session and loses reply/delete permissions.
        if has_credentials {
            return ApiError::unauthorized("登录已过期，请重新登录").into_response();
        }
        // Comments are publicly readable; posting and moderation still
        // require a signed-in account, and a valid session is preferred above.
        if request.method() == axum::http::Method::GET
            && path.starts_with("/themes/")
            && path.ends_with("/comments")
        {
            request.extensions_mut().insert(SessionInfo {
                user_id: String::new(),
                username: String::new(),
                display_name: String::new(),
                role: "guest".to_string(),
                status: "active".to_string(),
                email_verified: false,
                scope: "guest".to_string(),
            });
            return next.run(request).await;
        }
        return ApiError::unauthorized("请先登录").into_response();
    };
    if session.status == "rejected" || session.status == "disabled" {
        return ApiError::new(StatusCode::FORBIDDEN, "账号未启用").into_response();
    }
    request.extensions_mut().insert(session);
    next.run(request).await
}

async fn account_csrf_guard(request: axum::extract::Request, next: Next) -> Response {
    if request.method() == axum::http::Method::POST
        && auth::bearer_token(request.headers()).is_none()
    {
        let valid = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| value == CSRF_VALUE)
            .unwrap_or(false);
        if !valid {
            return ApiError::new(StatusCode::FORBIDDEN, "missing request header").into_response();
        }
    }
    next.run(request).await
}

async fn optional_session(state: &AppState, headers: &HeaderMap) -> Option<SessionInfo> {
    if let Some(token) = auth::bearer_token(headers) {
        if let Some(session) = state.sessions.validate(&token, "user").await {
            return Some(session);
        }
        if let Some(session) = state.sessions.validate(&token, "admin").await {
            return Some(session);
        }
    }
    if let Some(token) = auth::session_token(headers, auth::USER_SESSION_COOKIE) {
        if let Some(session) = state.sessions.validate(&token, "user").await {
            return Some(session);
        }
    }
    if let Some(token) = auth::session_token(headers, auth::ADMIN_SESSION_COOKIE) {
        if let Some(session) = state.sessions.validate(&token, "admin").await {
            return Some(session);
        }
    }
    None
}

pub(crate) fn rate_allowed(state: &AppState, key: &str, max: usize, window_secs: u64) -> bool {
    let mut rate = state.account_rate.lock().unwrap();
    let now = Instant::now();
    let hits = rate.entry(key.to_string()).or_default();
    hits.retain(|stamp| now.duration_since(*stamp) < Duration::from_secs(window_secs));
    if hits.len() >= max {
        return false;
    }
    hits.push(now);
    if rate.len() > 2000 {
        let keys: Vec<String> = rate.keys().take(400).cloned().collect();
        for key in keys {
            rate.remove(&key);
        }
    }
    true
}

fn valid_username(value: &str) -> bool {
    let value = value.trim();
    (3..=32).contains(&value.len())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn valid_display_name(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.chars().count() <= 40
}

fn valid_email(value: &str) -> bool {
    let value = value.trim();
    if value.len() < 5 || value.len() > 254 || value.contains(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = value.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn normalize_email(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn user_json(user: &User) -> Value {
    json!({
        "id": user.id,
        "username": user.username,
        "displayName": user.display_name,
        "email": user.email,
        "avatar": user.avatar,
        "role": user.role,
        "status": user.status,
        "emailVerified": user.email_verified,
        "rejectionReason": user.rejection_reason,
        "createdAt": user.created_at,
        "approvedAt": user.approved_at,
        "lastLoginAt": user.last_login_at,
    })
}

pub(crate) fn require_active(session: &SessionInfo) -> ApiResult<()> {
    if session.status != "active" {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "账号尚未通过管理员审核",
        ));
    }
    Ok(())
}

fn valid_password(value: &str) -> bool {
    (8..=128).contains(&value.len())
}

async fn config(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "email": mailer::configured(&state.paths),
        "github": oauth::GithubSettings::load(&state.paths).is_some(),
        "registration": true,
        "accountUrl": mailer::account_base_url(&state.paths),
    }))
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let ip = request_ip(&headers);
    if !rate_allowed(&state, &format!("register:{ip}"), 10, 3600) {
        return Err(ApiError::too_many("注册过于频繁，请稍后再试"));
    }
    if !mailer::configured(&state.paths) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "邮件服务尚未配置，暂时无法注册",
        ));
    }
    let value = json_body(body).await?;
    let username = value
        .get("username")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let display_name = value
        .get("displayName")
        .and_then(|value| value.as_str())
        .unwrap_or(&username)
        .trim()
        .to_string();
    let email = value
        .get("email")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if !valid_username(&username) {
        return Err(ApiError::bad("用户名需要 3-32 位字母、数字或 . _ -"));
    }
    if !valid_display_name(&display_name) {
        return Err(ApiError::bad("显示名称需要 1-40 个字符"));
    }
    if !valid_email(email) {
        return Err(ApiError::bad("请输入有效的邮箱地址"));
    }
    if !valid_password(password) {
        return Err(ApiError::bad("密码长度需要 8-128 位"));
    }
    let email = normalize_email(email);
    if accounts::username_exists(&state.db, &username)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict("用户名已被使用"));
    }
    if accounts::email_exists(&state.db, &email)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict("邮箱已被使用"));
    }
    let pass = password.to_string();
    let hash = tokio::task::spawn_blocking(move || auth::hash_admin_password(&pass))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::internal)?;
    let user = accounts::create_user(
        &state.db,
        accounts::NewUser {
            username: &username,
            display_name: &display_name,
            email: Some(&email),
            password_hash: Some(&hash),
            role: "user",
            status: "active",
            email_verified: false,
        },
    )
    .await
    .map_err(|error| ApiError::bad(error.to_string()))?;
    let token = accounts::issue_email_token(
        &state.db,
        Some(&user.id),
        &email,
        "verify_email",
        accounts::EMAIL_VERIFY_TTL_SECS,
    )
    .await
    .map_err(ApiError::internal)?;
    if let Err(error) = mailer::send_verification(&state.paths, &email, &token).await {
        tracing::warn!("verification email failed: {error:#}");
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "ok": true,
                "requiresVerification": true,
                "mailSent": false,
                "message": "账号已创建，但验证邮件发送失败，请稍后重试发送验证邮件",
            })),
        )
            .into_response());
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "requiresVerification": true,
            "mailSent": true,
            "message": "验证邮件已发送，请先验证邮箱",
        })),
    )
        .into_response())
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let ip = request_ip(&headers);
    if !rate_allowed(&state, &format!("login:{ip}"), 20, 300) {
        return Err(ApiError::too_many("尝试次数过多，请 5 分钟后再试"));
    }
    let value = json_body(body).await?;
    let login = value
        .get("login")
        .or_else(|| value.get("username"))
        .or_else(|| value.get("email"))
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if login.is_empty() || password.is_empty() {
        return Err(ApiError::bad("请输入账号和密码"));
    }
    let user = accounts::user_by_login(&state.db, &login)
        .await
        .map_err(ApiError::internal)?;
    let Some(user) = user else {
        return Err(ApiError::unauthorized("账号或密码错误"));
    };
    if user.status == "disabled" || user.status == "rejected" {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "账号未启用"));
    }
    if user.status == "email_pending" || !user.email_verified {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "请先验证邮箱后再登录"));
    }
    let hash = accounts::user_password_hash(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?
        .filter(|hash| !hash.trim().is_empty());
    let Some(hash) = hash else {
        return Err(ApiError::bad(
            "该账号尚未设置密码，请使用第三方登录或先在用户中心设置密码",
        ));
    };
    let pass = password.clone();
    let verified = tokio::task::spawn_blocking(move || bcrypt::verify(pass, &hash))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| ApiError::internal(error.to_string()))?;
    if !verified {
        return Err(ApiError::unauthorized("账号或密码错误"));
    }
    accounts::update_login_time(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?;
    let android = header_text(&headers, "x-client").eq_ignore_ascii_case("android");
    let admin_web = !android && user.is_admin();
    let ttl = if android {
        accounts::ANDROID_SESSION_TTL_SECS
    } else {
        crate::config::web_session_ttl_secs(&state.paths)
    };
    let token = state
        .sessions
        .create(
            &user.id,
            if admin_web { "admin" } else { "user" },
            ttl,
            &header_text(&headers, "user-agent"),
            &ip,
        )
        .await
        .map_err(ApiError::internal)?;
    let payload = json!({
        "ok": true,
        "admin": admin_web,
        "user": user_json(&user),
        "expiresIn": ttl,
        "token": if android { Some(token.clone()) } else { None },
    });
    if android {
        Ok((StatusCode::OK, Json(payload)).into_response())
    } else {
        let cookie = auth::session_cookie(
            if admin_web {
                auth::ADMIN_SESSION_COOKIE
            } else {
                auth::USER_SESSION_COOKIE
            },
            &token,
            "/",
            ttl,
            auth::request_is_secure(&headers),
        );
        let secure = auth::request_is_secure(&headers);
        let stale_cookie = auth::clear_session_cookie(
            if admin_web {
                auth::USER_SESSION_COOKIE
            } else {
                auth::ADMIN_SESSION_COOKIE
            },
            "/",
            secure,
        );
        let legacy_user_cookie =
            auth::clear_session_cookie(auth::USER_SESSION_COOKIE, "/account", secure);
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Cache-Control", "no-store")
            .header("Set-Cookie", cookie)
            .header("Set-Cookie", stale_cookie)
            .header("Set-Cookie", legacy_user_cookie)
            .body(axum::body::Body::from(payload.to_string()))
            .map_err(ApiError::internal)
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::bearer_token(&headers) {
        state.sessions.remove(&token).await;
    }
    if let Some(token) = auth::session_token(&headers, auth::USER_SESSION_COOKIE) {
        state.sessions.remove(&token).await;
    }
    if let Some(token) = auth::session_token(&headers, auth::ADMIN_SESSION_COOKIE) {
        state.sessions.remove(&token).await;
    }
    let secure = auth::request_is_secure(&headers);
    let user_cookie = auth::clear_session_cookie(auth::USER_SESSION_COOKIE, "/", secure);
    let legacy_user_cookie =
        auth::clear_session_cookie(auth::USER_SESSION_COOKIE, "/account", secure);
    let admin_cookie = auth::clear_session_cookie(auth::ADMIN_SESSION_COOKIE, "/", secure);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Set-Cookie", user_cookie)
        .header("Set-Cookie", legacy_user_cookie)
        .header("Set-Cookie", admin_cookie)
        .body(axum::body::Body::from(json!({"ok": true}).to_string()))
        .unwrap_or_else(|_| ApiError::internal("logout response").into_response())
}

async fn session_info(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let session = optional_session(&state, &headers).await;
    let user = match &session {
        Some(session) => accounts::user_by_id(&state.db, &session.user_id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let author = match &user {
        Some(user) => authors::active_author_for_user(&state.db, &user.id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    Json(json!({
        "ok": true,
        "authenticated": user.is_some(),
        "user": user.as_ref().map(user_json),
        "author": author,
        "email": mailer::configured(&state.paths),
        "github": oauth::GithubSettings::load(&state.paths).is_some(),
    }))
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    let current_hash = auth::bearer_token(&headers)
        .or_else(|| auth::session_token(&headers, auth::USER_SESSION_COOKIE))
        .or_else(|| auth::session_token(&headers, auth::ADMIN_SESSION_COOKIE))
        .map(|token| accounts::hash_token(&token));
    let sessions = accounts::user_sessions(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let sessions: Vec<Value> = sessions
        .into_iter()
        .map(|item| {
            let current = current_hash.as_deref() == Some(item.id.as_str());
            let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
            value["current"] = json!(current);
            value
        })
        .collect();
    Ok(Json(json!({"ok": true, "sessions": sessions})))
}

async fn revoke_session(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    let value = json_body(body).await?;
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::bad("无效的会话标识"));
    }
    let revoked = state
        .sessions
        .revoke_owned(&session.user_id, id)
        .await
        .map_err(ApiError::internal)?;
    if !revoked {
        return Err(ApiError::bad("会话不存在或已失效"));
    }
    if let Err(error) = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "session.revoke",
        Some("session"),
        None,
        None,
    )
    .await
    {
        tracing::warn!(?error, "failed to audit session revocation");
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn verify_email(State(state): State<AppState>, Query(query): Query<TokenQuery>) -> Response {
    let token = query.token.trim();
    if token.is_empty() {
        return Redirect::to("/account/login?verify=invalid").into_response();
    }
    if let Ok(Some(record)) = accounts::consume_email_token(&state.db, token, "verify_email").await
    {
        if let Some(user_id) = record.user_id {
            let _ = accounts::mark_email_verified(&state.db, &user_id).await;
        }
        return Redirect::to("/account/login?verified=1").into_response();
    }
    if let Ok(Some(record)) = accounts::consume_email_token(&state.db, token, "change_email").await
    {
        if let Some(user_id) = record.user_id {
            let _ = accounts::update_email(&state.db, &user_id, &record.email).await;
        }
        return Redirect::to("/account/login?email=verified").into_response();
    }
    Redirect::to("/account/login?verify=invalid").into_response()
}

async fn resend_verification(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let ip = request_ip(&headers);
    let value = json_body(body).await?;
    let email = normalize_email(
        value
            .get("email")
            .and_then(|value| value.as_str())
            .unwrap_or(""),
    );
    if valid_email(&email)
        && rate_allowed(&state, &format!("resend:{ip}:{email}"), 3, 600)
        && mailer::configured(&state.paths)
    {
        if let Ok(Some(user)) = accounts::user_by_email(&state.db, &email).await {
            if !user.email_verified && user.status != "disabled" && user.status != "rejected" {
                if let Ok(token) = accounts::issue_email_token(
                    &state.db,
                    Some(&user.id),
                    &email,
                    "verify_email",
                    accounts::EMAIL_VERIFY_TTL_SECS,
                )
                .await
                {
                    let _ = mailer::send_verification(&state.paths, &email, &token).await;
                }
            }
        }
    }
    Ok(Json(json!({
        "ok": true,
        "message": "如果该邮箱存在待验证账号，验证邮件已经发送",
    }))
    .into_response())
}

async fn forgot_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let ip = request_ip(&headers);
    let value = json_body(body).await?;
    let input = value
        .get("login")
        .or_else(|| value.get("username"))
        .or_else(|| value.get("email"))
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let candidate_email = normalize_email(&input);
    let user = if valid_email(&candidate_email) {
        accounts::user_by_email(&state.db, &candidate_email)
            .await
            .ok()
            .flatten()
    } else if input.is_empty() {
        None
    } else {
        accounts::user_by_login(&state.db, &input)
            .await
            .ok()
            .flatten()
    };
    let email = user
        .as_ref()
        .and_then(|user| user.email.clone())
        .filter(|value| valid_email(value))
        .unwrap_or_default();
    let rate_key = if email.is_empty() {
        format!("forgot:{ip}:{}", input.to_lowercase())
    } else {
        format!("forgot:{ip}:{email}")
    };
    if !email.is_empty()
        && rate_allowed(&state, &rate_key, 3, 600)
        && mailer::configured(&state.paths)
    {
        if let Some(user) = user {
            if user.status != "disabled" && user.status != "rejected" {
                if let Ok(token) = accounts::issue_email_token(
                    &state.db,
                    Some(&user.id),
                    &email,
                    "reset_password",
                    accounts::PASSWORD_RESET_TTL_SECS,
                )
                .await
                {
                    let _ = mailer::send_password_reset(&state.paths, &email, &token).await;
                }
            }
        }
    }
    Ok(Json(json!({
        "ok": true,
        "message": "如果该邮箱或账号存在，重置邮件已经发送",
    }))
    .into_response())
}

async fn reset_password(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let token = value
        .get("token")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if token.is_empty() {
        return Err(ApiError::bad("重置链接无效"));
    }
    if !valid_password(password) {
        return Err(ApiError::bad("密码长度需要 8-128 位"));
    }
    let record = accounts::consume_email_token(&state.db, token, "reset_password")
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::bad("重置链接无效或已过期"))?;
    let Some(user_id) = record.user_id else {
        return Err(ApiError::bad("重置链接无效"));
    };
    let pass = password.to_string();
    let hash = tokio::task::spawn_blocking(move || auth::hash_admin_password(&pass))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::internal)?;
    accounts::update_password(&state.db, &user_id, &hash)
        .await
        .map_err(ApiError::internal)?;
    state.sessions.revoke_user(&user_id).await;
    Ok(Json(json!({"ok": true, "message": "密码已重置，请重新登录"})).into_response())
}

#[derive(Deserialize)]
struct GithubStartQuery {
    link: Option<String>,
}

async fn github_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<GithubStartQuery>,
) -> ApiResult<Response> {
    let settings = oauth::GithubSettings::load(&state.paths)
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "GitHub 登录尚未配置"))?;
    let link_user = if query.link.as_deref() == Some("1") {
        optional_session(&state, &headers)
            .await
            .map(|session| session.user_id)
    } else {
        None
    };
    let state_value = oauth::create_state(&state.db, link_user.as_deref(), None)
        .await
        .map_err(ApiError::internal)?;
    let url = settings
        .authorize_url(&state_value)
        .map_err(ApiError::internal)?;
    Ok(Redirect::to(&url).into_response())
}

#[derive(Deserialize)]
struct GithubCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn github_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<GithubCallbackQuery>,
) -> Response {
    if query.error.is_some() {
        return Redirect::to("/account/login?github=denied").into_response();
    }
    let Some(code) = query.code else {
        return Redirect::to("/account/login?github=error").into_response();
    };
    let Some(state_value) = query.state else {
        return Redirect::to("/account/login?github=state").into_response();
    };
    let Some(oauth_state) = oauth::consume_state(&state.db, &state_value)
        .await
        .ok()
        .flatten()
    else {
        return Redirect::to("/account/login?github=state").into_response();
    };
    let Some(settings) = oauth::GithubSettings::load(&state.paths) else {
        return Redirect::to("/account/login?github=unconfigured").into_response();
    };
    let profile = match oauth::exchange(&state.http, &settings, &code).await {
        Ok(profile) => profile,
        Err(error) => {
            tracing::warn!("github oauth failed: {error:#}");
            return Redirect::to("/account/login?github=exchange").into_response();
        }
    };

    if let Some(user_id) = oauth_state.link_user_id {
        if let Ok(Some(existing)) =
            oauth::identity_user_id(&state.db, &profile.provider_user_id).await
        {
            if existing != user_id {
                return Redirect::to("/account/?github=conflict").into_response();
            }
        }
        if let Err(error) = oauth::link_identity(&state.db, &user_id, &profile).await {
            tracing::warn!("github link failed: {error:#}");
            return Redirect::to("/account/?github=error").into_response();
        }
        return Redirect::to("/account/?github=linked").into_response();
    }

    let mut user = if let Ok(Some(user_id)) =
        oauth::identity_user_id(&state.db, &profile.provider_user_id).await
    {
        accounts::user_by_id(&state.db, &user_id)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    if user.is_none() {
        if let Some(email) = &profile.email {
            if let Ok(Some(existing)) = accounts::user_by_email(&state.db, email).await {
                user = Some(existing);
            }
        }
    }
    let user = match user {
        Some(user) => {
            if let Err(error) = oauth::link_identity(&state.db, &user.id, &profile).await {
                tracing::warn!("github identity link failed: {error:#}");
                return Redirect::to("/account/login?github=error").into_response();
            }
            if !user.email_verified {
                if let Some(email) = &profile.email {
                    let _ = accounts::update_email(&state.db, &user.id, email).await;
                }
                let _ = accounts::mark_email_verified(&state.db, &user.id).await;
            }
            user
        }
        None => {
            let Some(email) = profile.email.clone() else {
                return Redirect::to("/account/login?github=email").into_response();
            };
            let base = sanitize_username(&profile.login);
            let username = match unique_username(&state, &base).await {
                Ok(value) => value,
                Err(_) => return Redirect::to("/account/login?github=error").into_response(),
            };
            let display_name = if profile.name.trim().is_empty() {
                profile.login.clone()
            } else {
                profile.name.clone()
            };
            match accounts::create_user(
                &state.db,
                accounts::NewUser {
                    username: &username,
                    display_name: &display_name,
                    email: Some(&email),
                    password_hash: None,
                    role: "user",
                    status: "pending",
                    email_verified: true,
                },
            )
            .await
            {
                Ok(user) => {
                    let _ = oauth::link_identity(&state.db, &user.id, &profile).await;
                    user
                }
                Err(error) => {
                    tracing::warn!("github user creation failed: {error:#}");
                    return Redirect::to("/account/login?github=error").into_response();
                }
            }
        }
    };
    if user.status == "disabled" || user.status == "rejected" {
        return Redirect::to("/account/login?github=disabled").into_response();
    }
    let ip = request_ip(&headers);
    let token = match state
        .sessions
        .create(
            &user.id,
            "user",
            crate::config::web_session_ttl_secs(&state.paths),
            &header_text(&headers, "user-agent"),
            &ip,
        )
        .await
    {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!("github session failed: {error:#}");
            return Redirect::to("/account/login?github=error").into_response();
        }
    };
    let secure = auth::request_is_secure(&headers);
    let cookie = auth::session_cookie(
        auth::USER_SESSION_COOKIE,
        &token,
        "/",
        crate::config::web_session_ttl_secs(&state.paths),
        secure,
    );
    let legacy_cookie = auth::clear_session_cookie(auth::USER_SESSION_COOKIE, "/account", secure);
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header("Location", "/account/?github=1")
        .header("Set-Cookie", cookie)
        .header("Set-Cookie", legacy_cookie)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| Redirect::to("/account/").into_response())
}

fn sanitize_username(value: &str) -> String {
    let mut result: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .collect();
    if result.len() < 3 {
        result.push_str("user");
    }
    result.truncate(24);
    if result.is_empty() {
        "user".to_string()
    } else {
        result
    }
}

async fn unique_username(state: &AppState, base: &str) -> anyhow::Result<String> {
    for _ in 0..20 {
        let candidate = if base.len() >= 3 {
            format!("{}-{}", base, random_hex(2))
        } else {
            format!("user-{}", random_hex(3))
        };
        if !accounts::username_exists(&state.db, &candidate).await? {
            return Ok(candidate);
        }
    }
    Ok(format!("user-{}", random_hex(6)))
}

async fn github_unlink(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Response> {
    let hash = accounts::user_password_hash(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    if hash.is_none() {
        return Err(ApiError::bad("请先设置密码，再解绑 GitHub"));
    }
    let removed = oauth::unlink_github(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    if !removed {
        return Err(ApiError::not_found("尚未绑定 GitHub"));
    }
    Ok(Json(json!({"ok": true, "message": "已解绑 GitHub"})).into_response())
}

#[derive(Deserialize)]
struct AuthorHashBody {
    #[serde(rename = "authorHash", default)]
    author_hash: String,
}

async fn my_follows(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let authors = follows::following_authors(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let followers = follows::follower_count(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let following = authors.len() as i64;
    Ok(Json(json!({
        "ok": true,
        "authors": authors,
        "followers": followers,
        "following": following,
    })))
}

async fn preferences_value(state: &AppState, user_id: &str) -> ApiResult<Value> {
    let user_id = user_id.to_string();
    let row = state
        .db
        .call(move |conn| {
            let row = conn
                .query_row(
                    "SELECT show_r18, show_r18_set, consent_version, consent_at
                     FROM user_preferences WHERE user_id = ?1",
                    [user_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
        .map_err(ApiError::internal)?;
    let (show_r18, show_r18_set, consent_version, consent_at) = row.unwrap_or((0, 0, 0, None));
    Ok(json!({
        "ok": true,
        "showR18": show_r18 != 0,
        "showR18Set": show_r18_set != 0,
        "consentVersion": consent_version,
        "consentAt": consent_at,
    }))
}

async fn get_preferences(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    Ok(Json(preferences_value(&state, &session.user_id).await?))
}

async fn update_preferences(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let show_r18 = value
        .get("showR18")
        .and_then(Value::as_bool)
        .map(|value| value as i64);
    let consent_version = value
        .get("consentVersion")
        .and_then(Value::as_i64)
        .map(|value| value.max(0));
    if show_r18.is_none() && consent_version.is_none() {
        return Err(ApiError::bad("没有需要保存的设置"));
    }
    let user_id = session.user_id.clone();
    let now = now_iso();
    let consent_at = consent_version
        .filter(|value| *value > 0)
        .map(|_| now.clone());
    state
        .db
        .call(move |conn| {
            conn.execute(
                "INSERT INTO user_preferences(user_id, show_r18, show_r18_set, consent_version, consent_at, updated_at)
                 VALUES (?1, COALESCE(?2, 0), CASE WHEN ?2 IS NULL THEN 0 ELSE 1 END, COALESCE(?3, 0), ?4, ?5)
                 ON CONFLICT(user_id) DO UPDATE SET
                    show_r18 = COALESCE(?2, user_preferences.show_r18),
                    show_r18_set = CASE
                        WHEN ?2 IS NULL THEN user_preferences.show_r18_set
                        ELSE 1
                    END,
                    consent_version = COALESCE(?3, user_preferences.consent_version),
                    consent_at = CASE
                        WHEN ?3 IS NULL THEN user_preferences.consent_at
                        WHEN ?3 = 0 THEN NULL
                        WHEN user_preferences.consent_version = 0
                            OR user_preferences.consent_at IS NULL THEN ?4
                        ELSE user_preferences.consent_at
                    END,
                    updated_at = ?5",
                params![user_id, show_r18, consent_version, consent_at, now],
            )?;
            Ok(())
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(preferences_value(&state, &session.user_id).await?))
}

async fn follow_author(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let author_hash = value
        .get("authorHash")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !valid_author_hash(&author_hash) {
        return Err(ApiError::bad("作者标识无效"));
    }
    if !follows::author_exists(&state.db, &author_hash)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::not_found("作者不存在"));
    }
    follows::follow(&state.db, &session.user_id, &author_hash)
        .await
        .map_err(ApiError::internal)?;
    let followers = follows::author_follower_count(&state.db, &author_hash)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "following": true,
        "followers": followers,
    })))
}

async fn unfollow_author(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let author_hash = value
        .get("authorHash")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !valid_author_hash(&author_hash) {
        return Err(ApiError::bad("作者标识无效"));
    }
    follows::unfollow(&state.db, &session.user_id, &author_hash)
        .await
        .map_err(ApiError::internal)?;
    let followers = follows::author_follower_count(&state.db, &author_hash)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "following": false,
        "followers": followers,
    })))
}

const LEADERBOARD_FOLLOWER_WEIGHT: i64 = 10;

#[derive(Debug, Deserialize, Default)]
struct LeaderboardQuery {
    limit: Option<usize>,
}

/// Author love leaderboard for the user center: followers × 10 + the summed
/// heat of every theme owned by the account's active author identities. Theme
/// heat is attributed through the catalog `authorHash`, so themes under an
/// unlinked identity never rank, and R18 themes only count for viewers who
/// opted into R18 — the same visibility rule as the discover pool.
async fn author_leaderboard(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<LeaderboardQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(10).clamp(1, 20);
    let users = follows::leaderboard_users(&state.db)
        .await
        .map_err(ApiError::internal)?;
    if users.is_empty() {
        return Ok(Json(json!({
            "ok": true,
            "rule": "粉丝数 × 10 + 主题热度合计",
            "limit": limit,
            "authors": [],
        })));
    }
    let author_rows = follows::leaderboard_authors(&state.db)
        .await
        .map_err(ApiError::internal)?;
    let author_info: HashMap<&str, &follows::LeaderboardAuthor> = author_rows
        .iter()
        .map(|author| (author.hash.as_str(), author))
        .collect();

    #[derive(Default)]
    struct HashAgg {
        heat: i64,
        themes: i64,
        avatar_path: Option<String>,
    }
    let mut per_hash: HashMap<String, HashAgg> = HashMap::new();
    if let Some(catalog) = crate::catalog::read_catalog(&state.paths) {
        let show_r18 = user_show_r18(&state, &session.user_id).await?;
        let stats = state.theme_stats();
        for theme in &catalog.themes {
            let Some(hash) = theme.author_hash.as_deref() else {
                continue;
            };
            if !author_info.contains_key(hash) {
                continue;
            }
            if !show_r18 && (theme.r18 || crate::catalog::tags_mark_r18(&theme.tags)) {
                continue;
            }
            let heat = stats
                .get(&theme.id)
                .map(|item| item.heat as i64)
                .unwrap_or(theme.heat as i64);
            let agg = per_hash.entry(hash.to_string()).or_default();
            agg.heat += heat;
            agg.themes += 1;
            if agg.avatar_path.is_none() {
                agg.avatar_path = theme
                    .avatar_path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .map(str::to_string);
            }
        }
    }

    let mut rows: Vec<Value> = Vec::new();
    for user in users {
        let mut heat = 0i64;
        let mut themes = 0i64;
        let mut avatar_path: Option<String> = None;
        let mut follow_hash: Option<&String> = None;
        for hash in &user.author_hashes {
            if let Some(agg) = per_hash.get(hash) {
                heat += agg.heat;
                themes += agg.themes;
                if avatar_path.is_none() {
                    avatar_path = agg.avatar_path.clone();
                }
            }
            // The follow button targets one identity: prefer the one carrying
            // the most themes, falling back to the oldest identity.
            match follow_hash {
                None => follow_hash = Some(hash),
                Some(current) => {
                    let current_themes = per_hash.get(current).map(|item| item.themes).unwrap_or(0);
                    let current_created = author_info
                        .get(current.as_str())
                        .map(|item| item.created_at.as_str())
                        .unwrap_or("");
                    let candidate_themes = per_hash.get(hash).map(|item| item.themes).unwrap_or(0);
                    let candidate_created = author_info
                        .get(hash.as_str())
                        .map(|item| item.created_at.as_str())
                        .unwrap_or("");
                    if candidate_themes > current_themes
                        || (candidate_themes == current_themes
                            && candidate_created < current_created)
                    {
                        follow_hash = Some(hash);
                    }
                }
            }
        }
        if user.followers == 0 && heat == 0 {
            continue;
        }
        let selected = follow_hash.map(|hash| hash.as_str());
        let name = selected
            .and_then(|hash| {
                author_info
                    .get(hash)
                    .and_then(|item| item.display_name.as_deref())
            })
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if user.display_name.trim().is_empty() {
                    user.username.clone()
                } else {
                    user.display_name.clone()
                }
            });
        let avatar_url = user
            .avatar
            .as_deref()
            .filter(|value| value.starts_with("avatars/users/"))
            .map(|_| format!("/account/api/users/{}/avatar", user.user_id))
            .or_else(|| {
                avatar_path.map(|path| format!("/account/api/preview?path={}", quote_query(&path)))
            })
            .unwrap_or_default();
        rows.push(json!({
            "userId": user.user_id,
            "name": name,
            "authorHash": selected.unwrap_or(""),
            "avatarUrl": avatar_url,
            "fans": user.followers,
            "themes": themes,
            "heat": heat,
            "score": user.followers * LEADERBOARD_FOLLOWER_WEIGHT + heat,
        }));
    }
    rows.sort_by(|left, right| {
        let score = right["score"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&left["score"].as_i64().unwrap_or(0));
        score
            .then_with(|| {
                right["fans"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&left["fans"].as_i64().unwrap_or(0))
            })
            .then_with(|| {
                right["heat"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&left["heat"].as_i64().unwrap_or(0))
            })
            .then_with(|| {
                left["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(right["name"].as_str().unwrap_or(""))
            })
    });
    rows.truncate(limit);
    Ok(Json(json!({
        "ok": true,
        "rule": "粉丝数 × 10 + 主题热度合计",
        "limit": limit,
        "authors": rows,
    })))
}

const DEVICE_CODE_TTL_SECS: u64 = 10 * 60;
const DEVICE_CODE_INTERVAL_SECS: u64 = 2;

fn random_device_user_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

async fn unique_device_user_code(state: &AppState) -> ApiResult<String> {
    for _ in 0..12 {
        let code = random_device_user_code();
        let candidate = code.clone();
        let exists: bool = state
            .db
            .call(move |conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM device_login_codes WHERE user_code = ?1",
                    [candidate],
                    |row| row.get(0),
                )?;
                Ok(count > 0)
            })
            .await
            .map_err(ApiError::internal)?;
        if !exists {
            return Ok(code);
        }
    }
    Err(ApiError::internal("无法生成一次性密钥"))
}

async fn device_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ip = request_ip(&headers);
    if !rate_allowed(&state, &format!("device-start:{ip}"), 10, 300) {
        return Err(ApiError::too_many("操作过于频繁，请稍后再试"));
    }
    let value = json_body(body)
        .await
        .unwrap_or_else(|_| Value::Object(Default::default()));
    let device_name = value
        .get("deviceName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let user_agent = header_text(&headers, "user-agent");
    let stored_agent = if device_name.is_empty() {
        user_agent
    } else {
        format!("{user_agent} ({device_name})")
    };
    let device_code = random_hex(32);
    let device_code_hash = sha256_hex(device_code.as_bytes());
    let user_code = unique_device_user_code(&state).await?;
    let now = now_iso();
    let expires_at = unix_now() as i64 + DEVICE_CODE_TTL_SECS as i64;
    let hash_for_db = device_code_hash.clone();
    let code_for_db = user_code.clone();
    let agent_for_db = stored_agent.clone();
    let ip_for_db = ip.clone();
    state
        .db
        .call(move |conn| {
            conn.execute(
                "DELETE FROM device_login_codes WHERE expires_at <= ?1",
                [unix_now() as i64],
            )?;
            conn.execute(
                "INSERT INTO device_login_codes(
                    device_code_hash, user_code, status, user_agent, ip, created_at, expires_at
                 ) VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6)",
                params![
                    hash_for_db,
                    code_for_db,
                    agent_for_db,
                    ip_for_db,
                    now,
                    expires_at
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(ApiError::internal)?;
    let verify_url = format!(
        "{}device?code={}",
        mailer::account_base_url(&state.paths),
        user_code
    );
    Ok(Json(json!({
        "ok": true,
        "deviceCode": device_code,
        "userCode": user_code,
        "verifyUrl": verify_url,
        "expiresIn": DEVICE_CODE_TTL_SECS,
        "interval": DEVICE_CODE_INTERVAL_SECS,
    })))
}

#[derive(Deserialize)]
struct DevicePollQuery {
    #[serde(rename = "deviceCode", default)]
    device_code: String,
}

async fn device_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DevicePollQuery>,
) -> ApiResult<Json<Value>> {
    let ip = request_ip(&headers);
    if !rate_allowed(&state, &format!("device-poll:{ip}"), 600, 600) {
        return Err(ApiError::too_many("轮询过于频繁"));
    }
    let device_code = query.device_code.trim().to_string();
    if device_code.is_empty() {
        return Err(ApiError::bad("缺少登录密钥"));
    }
    let device_code_hash = sha256_hex(device_code.as_bytes());
    let row = state
        .db
        .call(move |conn| {
            let row = conn
                .query_row(
                    "SELECT status, user_id, expires_at, COALESCE(user_agent, ''), consumed_at
                     FROM device_login_codes WHERE device_code_hash = ?1",
                    [device_code_hash],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("登录密钥无效或已过期"))?;
    let (status, user_id, expires_at, stored_agent, consumed_at) = row;
    if consumed_at.is_some() {
        return Ok(Json(json!({"ok": true, "status": "consumed"})));
    }
    if expires_at <= unix_now() as i64 {
        return Ok(Json(json!({"ok": true, "status": "expired"})));
    }
    if status != "approved" {
        return Ok(Json(json!({"ok": true, "status": "pending"})));
    }
    let Some(user_id) = user_id else {
        return Ok(Json(json!({"ok": true, "status": "pending"})));
    };
    let updated = {
        let device_code_hash = sha256_hex(device_code.as_bytes());
        let consumed_at = now_iso();
        state
            .db
            .call(move |conn| {
                let changed = conn.execute(
                    "UPDATE device_login_codes
                     SET consumed_at = ?1
                     WHERE device_code_hash = ?2
                       AND status = 'approved'
                       AND consumed_at IS NULL",
                    params![consumed_at, device_code_hash],
                )?;
                Ok(changed)
            })
            .await
            .map_err(ApiError::internal)?
    };
    if updated == 0 {
        return Ok(Json(json!({"ok": true, "status": "consumed"})));
    }
    let user = accounts::user_by_id(&state.db, &user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    if user.status == "disabled" || user.status == "rejected" {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "账号未启用"));
    }
    let ttl = accounts::ANDROID_SESSION_TTL_SECS;
    let agent = if stored_agent.is_empty() {
        header_text(&headers, "user-agent")
    } else {
        stored_agent
    };
    let token = state
        .sessions
        .create(&user.id, "user", ttl, &agent, &ip)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "status": "approved",
        "user": user_json(&user),
        "expiresIn": ttl,
        "token": token,
    })))
}

#[derive(Deserialize)]
struct DeviceInfoQuery {
    #[serde(default)]
    code: String,
}

async fn device_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DeviceInfoQuery>,
) -> ApiResult<Json<Value>> {
    let ip = request_ip(&headers);
    if !rate_allowed(&state, &format!("device-info:{ip}"), 120, 300) {
        return Err(ApiError::too_many("请求过于频繁"));
    }
    let user_code = query.code.trim().to_ascii_uppercase();
    if user_code.is_empty() {
        return Err(ApiError::bad("缺少一次性密钥"));
    }
    let row = state
        .db
        .call(move |conn| {
            let row = conn
                .query_row(
                    "SELECT status, created_at, expires_at, COALESCE(user_agent, ''), consumed_at
                     FROM device_login_codes WHERE user_code = ?1",
                    [user_code],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("一次性密钥无效"))?;
    let (status, created_at, expires_at, user_agent, consumed_at) = row;
    let status = if consumed_at.is_some() {
        "consumed"
    } else if expires_at <= unix_now() as i64 {
        "expired"
    } else {
        status.as_str()
    };
    Ok(Json(json!({
        "ok": true,
        "status": status,
        "createdAt": created_at,
        "expiresAt": expires_at,
        "userAgent": user_agent,
    })))
}

#[derive(Deserialize)]
struct DeviceApproveBody {
    #[serde(rename = "userCode", default)]
    user_code: String,
}

async fn device_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ip = request_ip(&headers);
    if !rate_allowed(
        &state,
        &format!("device-approve:{}:{ip}", session.user_id),
        30,
        300,
    ) {
        return Err(ApiError::too_many("操作过于频繁，请稍后再试"));
    }
    let value = json_body(body).await?;
    let user_code = value
        .get("userCode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    if user_code.is_empty() {
        return Err(ApiError::bad("缺少一次性密钥"));
    }
    let user_id = session.user_id.clone();
    let approved_at = now_iso();
    let changed = state
        .db
        .call(move |conn| {
            let changed = conn.execute(
                "UPDATE device_login_codes
                 SET status = 'approved', user_id = ?1, approved_at = ?2
                 WHERE user_code = ?3
                   AND status = 'pending'
                   AND expires_at > ?4",
                params![user_id, approved_at, user_code, unix_now() as i64],
            )?;
            Ok(changed)
        })
        .await
        .map_err(ApiError::internal)?;
    if changed == 0 {
        return Err(ApiError::bad("一次性密钥无效、已过期或已被使用"));
    }
    Ok(Json(json!({
        "ok": true,
        "message": "已确认登录，可以返回应用",
    })))
}

async fn profile(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let user = accounts::user_by_id(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    let profile_authors = authors::authors_for_user(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?;
    let mut author_values = Vec::with_capacity(profile_authors.len());
    for author in &profile_authors {
        let mut value = serde_json::to_value(author).map_err(ApiError::internal)?;
        if let Some(avatar) = crate::catalog::author_avatar_key(
            &state.store,
            &state.paths,
            &author.hash,
            author.display_name.as_deref(),
        )
        .await
        {
            if let Some(object) = value.as_object_mut() {
                object.insert("avatarPath".to_string(), json!(avatar));
            }
        }
        author_values.push(value);
    }
    let active_author = author_values
        .iter()
        .find(|value| value.get("status").and_then(Value::as_str) == Some("active"))
        .cloned();
    let github = oauth::github_identity(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?;
    let followers = follows::follower_count(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?;
    let following = follows::following_count(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?;
    let password_configured = accounts::user_password_hash(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?
        .filter(|hash| !hash.trim().is_empty())
        .is_some();
    Ok(Json(json!({
        "ok": true,
        "user": user_json(&user),
        "authors": author_values,
        "activeAuthor": active_author,
        "github": github,
        "followers": followers,
        "following": following,
        "passwordConfigured": password_configured,
        "mailConfigured": mailer::configured(&state.paths),
        "githubConfigured": oauth::GithubSettings::load(&state.paths).is_some(),
        "managerVersionCode": crate::manager_version::current(&state.paths).await,
    })))
}

async fn update_profile(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let username = value
        .get("username")
        .and_then(|value| value.as_str())
        .unwrap_or(&session.username)
        .trim()
        .to_string();
    let display_name = value
        .get("displayName")
        .and_then(|value| value.as_str())
        .unwrap_or(&session.display_name)
        .trim()
        .to_string();
    if !valid_username(&username) {
        return Err(ApiError::bad("用户名需要 3-32 位字母、数字或 . _ -"));
    }
    if !valid_display_name(&display_name) {
        return Err(ApiError::bad("显示名称需要 1-40 个字符"));
    }
    accounts::update_profile(&state.db, &session.user_id, &username, &display_name, None)
        .await
        .map_err(|error| ApiError::conflict(error.to_string()))?;
    let user = accounts::user_by_id(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    Ok(Json(json!({"ok": true, "user": user_json(&user)})))
}

async fn change_password(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let current = value
        .get("currentPassword")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if !valid_password(password) {
        return Err(ApiError::bad("密码长度需要 8-128 位"));
    }
    let existing = accounts::user_password_hash(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
        .filter(|hash| !hash.trim().is_empty());
    let pass = password.to_string();
    let new_hash = tokio::task::spawn_blocking(move || auth::hash_admin_password(&pass))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::internal)?;
    match existing {
        Some(hash) => {
            let current = current.to_string();
            let verified = tokio::task::spawn_blocking(move || bcrypt::verify(current, &hash))
                .await
                .map_err(|error| ApiError::internal(error.to_string()))?
                .map_err(|error| ApiError::internal(error.to_string()))?;
            if !verified {
                return Err(ApiError::bad("当前密码不正确"));
            }
            accounts::update_password(&state.db, &session.user_id, &new_hash)
                .await
                .map_err(ApiError::internal)?;
            state.sessions.revoke_user(&session.user_id).await;
            Ok(Json(
                json!({"ok": true, "message": "密码已更新，请重新登录", "relogin": true}),
            ))
        }
        None => {
            // Accounts created through GitHub have no password yet. A signed-in session is
            // enough to set the first one, and the current session stays usable.
            accounts::update_password(&state.db, &session.user_id, &new_hash)
                .await
                .map_err(ApiError::internal)?;
            Ok(Json(
                json!({"ok": true, "message": "密码已设置", "relogin": false}),
            ))
        }
    }
}

async fn change_email(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    if !mailer::configured(&state.paths) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "邮件服务尚未配置",
        ));
    }
    let value = json_body(body).await?;
    let email = normalize_email(
        value
            .get("email")
            .and_then(|value| value.as_str())
            .unwrap_or(""),
    );
    if !valid_email(&email) {
        return Err(ApiError::bad("请输入有效的邮箱地址"));
    }
    if accounts::email_exists(&state.db, &email)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict("邮箱已被使用"));
    }
    let token = accounts::issue_email_token(
        &state.db,
        Some(&session.user_id),
        &email,
        "change_email",
        accounts::EMAIL_VERIFY_TTL_SECS,
    )
    .await
    .map_err(ApiError::internal)?;
    mailer::send_verification(&state.paths, &email, &token)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, error.to_string()))?;
    Ok(Json(
        json!({"ok": true, "message": "验证邮件已发送到新邮箱"}),
    ))
}

async fn upload_avatar(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let mut data: Option<Vec<u8>> = None;
    let mut ext = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad(error.to_string()))?
    {
        if field.name() != Some("avatar") {
            continue;
        }
        ext = field
            .file_name()
            .and_then(|name| std::path::Path::new(name).extension())
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", value.to_ascii_lowercase()))
            .unwrap_or_default();
        let bytes = field
            .bytes()
            .await
            .map_err(|error| ApiError::bad(error.to_string()))?;
        if bytes.len() > crate::config::MAX_IMAGE_BYTES {
            return Err(ApiError::bad("头像不能超过 2 MB"));
        }
        data = Some(bytes.to_vec());
    }
    let Some(data) = data else {
        return Err(ApiError::bad("请选择头像文件"));
    };
    let (optimized, ext) =
        tokio::task::spawn_blocking(move || images::optimize_image(data, &ext, AVATAR_MAX_EDGE))
            .await
            .map_err(|error| ApiError::internal(error.to_string()))?
            .map_err(|error| ApiError::bad(error.to_string()))?;
    let key = format!("avatars/users/{}{}", session.user_id, ext);
    let content_type = content_type_for(&key);
    state
        .store
        .put(&key, &optimized, &content_type)
        .await
        .map_err(ApiError::internal)?;
    let old = accounts::user_by_id(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
        .and_then(|user| user.avatar);
    if let Some(old) = old {
        if old != key {
            let _ = state.store.delete(&old).await;
        }
    }
    accounts::set_avatar(&state.db, &session.user_id, Some(&key))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "avatar": key})))
}

async fn my_authors(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let list = authors::authors_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let requests = authors::author_requests(&state.db, None, Some(&session.user_id))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "authors": list,
        "activeAuthor": list.iter().find(|author| author.status == "active"),
        "requests": requests,
    })))
}

async fn my_author_requests(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let requests = authors::author_requests(&state.db, None, Some(&session.user_id))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "requests": requests})))
}

async fn create_author_request(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    let value = json_body(body).await?;
    let kind = value
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let author_id = value
        .get("authorId")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let display_name = value
        .get("displayName")
        .and_then(|value| value.as_str())
        .unwrap_or(&session.display_name)
        .trim()
        .to_string();
    let note_text = value
        .get("note")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if note_text.chars().count() > 500 {
        return Err(ApiError::bad("申请说明不能超过 500 个字符"));
    }
    let note = if note_text.is_empty() {
        None
    } else {
        Some(note_text.as_str())
    };
    if !matches!(kind.as_str(), "bind" | "new" | "reset") {
        return Err(ApiError::bad("未知的作者申请类型"));
    }
    if !valid_display_name(&display_name) {
        return Err(ApiError::bad("作者名称需要 1-40 个字符"));
    }
    if authors::has_pending_request(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict("你还有待审核的作者申请，请等待审核完成"));
    }
    let existing = authors::authors_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let reapplying = matches!(kind.as_str(), "bind" | "new")
        && existing
            .iter()
            .any(|author| author.status == "revoked" || author.status == "retired");
    if reapplying && note_text.chars().count() < 10 {
        return Err(ApiError::bad(
            "作者身份曾被撤销，重新申请需要填写至少 10 字的说明",
        ));
    }
    if kind == "reset" && note_text.chars().count() < 5 {
        return Err(ApiError::bad("重置作者 ID 需要填写至少 5 字的说明"));
    }
    if kind == "new" && existing.iter().any(|author| author.status == "active") {
        return Err(ApiError::conflict(
            "当前已有作者身份，如需换号请申请重置作者 ID",
        ));
    }
    if kind == "reset" {
        let active = existing
            .iter()
            .find(|author| author.status == "active")
            .ok_or_else(|| ApiError::bad("当前没有可重置的作者身份"))?;
        let id = authors::create_author_request(
            &state.db,
            &session.user_id,
            "reset",
            Some(&active.hash),
            &active.display_name.clone().unwrap_or(display_name),
            note,
        )
        .await
        .map_err(ApiError::internal)?;
        return Ok(Json(
            json!({"ok": true, "requestId": id, "status": "pending"}),
        ));
    }

    if kind == "bind" {
        if !valid_author_id(&author_id) {
            return Err(ApiError::bad("作者 ID 需要 16-128 位字母、数字或 . _ ~ -"));
        }
        let hash = author_key(&author_id);
        let fingerprint = author_fingerprint(&hash);
        match authors::author_by_hash(&state.db, &hash)
            .await
            .map_err(ApiError::internal)?
        {
            Some(author) if author.user_id.as_deref() == Some(session.user_id.as_str()) => {
                return Ok(Json(json!({
                    "ok": true,
                    "alreadyBound": true,
                    "status": author.status,
                    "author": author,
                })));
            }
            Some(author) if author.user_id.is_some() => {
                return Err(ApiError::conflict("该作者 ID 已绑定其他账号"));
            }
            Some(_) => {
                let pending =
                    authors::author_requests(&state.db, Some("pending"), Some(&session.user_id))
                        .await
                        .map_err(ApiError::internal)?;
                if pending.iter().any(|item| {
                    item.get("authorHash").and_then(|v| v.as_str()) == Some(hash.as_str())
                }) {
                    return Err(ApiError::conflict("该作者 ID 已在审核中"));
                }
                let id = authors::create_author_request(
                    &state.db,
                    &session.user_id,
                    "bind",
                    Some(&hash),
                    &display_name,
                    note,
                )
                .await
                .map_err(ApiError::internal)?;
                return Ok(Json(
                    json!({"ok": true, "requestId": id, "status": "pending"}),
                ));
            }
            None => {
                authors::create_author(
                    &state.db,
                    &hash,
                    &fingerprint,
                    &display_name,
                    Some(&session.user_id),
                    "pending",
                )
                .await
                .map_err(ApiError::internal)?;
                let _ = authors::set_auto_publish(&state.db, &hash, false).await;
                let id = authors::create_author_request(
                    &state.db,
                    &session.user_id,
                    "new",
                    Some(&hash),
                    &display_name,
                    note,
                )
                .await
                .map_err(ApiError::internal)?;
                return Ok(Json(
                    json!({"ok": true, "requestId": id, "status": "pending"}),
                ));
            }
        }
    }

    // kind == "new"
    let provided = if author_id.is_empty() {
        None
    } else if valid_author_id(&author_id) {
        Some(author_id)
    } else {
        return Err(ApiError::bad("作者 ID 需要 16-128 位字母、数字或 . _ ~ -"));
    };
    let raw_id = provided.clone().unwrap_or_else(|| random_hex(24));
    let hash = author_key(&raw_id);
    if let Some(existing) = authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
    {
        if existing.user_id.is_some() {
            return Err(ApiError::conflict("该作者 ID 已被绑定"));
        }
    }
    authors::create_author(
        &state.db,
        &hash,
        &author_fingerprint(&hash),
        &display_name,
        Some(&session.user_id),
        "pending",
    )
    .await
    .map_err(ApiError::internal)?;
    let _ = authors::set_auto_publish(&state.db, &hash, false).await;
    let id = authors::create_author_request(
        &state.db,
        &session.user_id,
        "new",
        Some(&hash),
        &display_name,
        note,
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "requestId": id,
        "status": "pending",
        "authorId": if provided.is_none() { Some(raw_id) } else { None },
    })))
}

async fn generate_reset_author(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    let request = authors::author_request_by_id(&state.db, &id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("申请不存在"))?;
    if request.user_id != session.user_id {
        return Err(ApiError::not_found("申请不存在"));
    }
    if request.kind != "reset" || request.status != "approved" {
        return Err(ApiError::bad("该申请尚未批准或不是重置申请"));
    }
    let (raw_id, author) = authors::reset_author(&state.db, &session.user_id, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    if let Ok(Some(theme_id)) = authors::bind_latest_theme(
        &state.db,
        &state.paths,
        &session.user_id,
        &author.hash,
        author.display_name.as_deref(),
    )
    .await
    {
        if let Err(error) = crate::catalog::rebuild_catalog(&state.store, &state.paths).await {
            tracing::warn!("author theme rebind catalog rebuild failed: {error:#}");
        }
        state.refresh_withdrawn_assets();
        let _ = accounts::audit(
            &state.db,
            Some(&session.user_id),
            "author.bind_latest_theme",
            Some("theme"),
            Some(&theme_id),
            Some(&author.hash),
        )
        .await;
    }
    if let Err(error) =
        authors::schedule_grace_if_needed(&state.db, &state.paths, &author.hash).await
    {
        tracing::warn!("author grace scheduling failed: {error:#}");
    }
    authors::review_author_request(
        &state.db,
        &id,
        "approved",
        &session.user_id,
        None,
        Some(&author.hash),
    )
    .await
    .map_err(ApiError::internal)?;
    accounts::audit(
        &state.db,
        Some(&session.user_id),
        "author.reset",
        Some("author"),
        Some(&author.hash),
        None,
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "authorId": raw_id,
        "author": author,
        "message": "新作者 ID 只显示一次，请妥善保存",
    })))
}

#[derive(Debug, Deserialize, Default)]
struct DiscoverQuery {
    limit: Option<usize>,
    exclude: Option<String>,
    author: Option<String>,
    /// `hot` sorts by heat instead of shuffling; the user center's popular card
    /// uses this so it mirrors the admin overview ranking.
    sort: Option<String>,
    /// `1` keeps the signed-in user's own themes in the pool (popular rankings).
    #[serde(rename = "includeOwn")]
    include_own: Option<String>,
}

/// Theme ids owned by the signed-in account, used to keep "别人的主题"
/// previews free of the user's own uploads.
async fn owned_theme_ids(state: &AppState, session: &SessionInfo) -> ApiResult<HashSet<String>> {
    if session.role == "admin" {
        return Ok(HashSet::new());
    }
    let profile_authors = authors::authors_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let hashes: HashSet<String> = profile_authors
        .into_iter()
        .map(|author| author.hash)
        .collect();
    let index = state.meta_index().await;
    Ok(index.owned_ids(&hashes, &session.username))
}

pub(crate) async fn user_show_r18(state: &AppState, user_id: &str) -> ApiResult<bool> {
    let user_id = user_id.to_string();
    let enabled = state
        .db
        .call(move |conn| {
            let value: Option<i64> = conn
                .query_row(
                    "SELECT show_r18 FROM user_preferences WHERE user_id = ?1",
                    [user_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value.unwrap_or(0) != 0)
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(enabled)
}

pub(crate) fn discover_theme_json(
    theme: &crate::models::CatalogEntry,
    stats: &HashMap<String, crate::db::ThemeStats>,
) -> Value {
    let theme_stats = stats.get(&theme.id).copied().unwrap_or_default();
    // `v` makes the URL content-versioned so a replaced preview is fetched again,
    // while unchanged assets can be cached aggressively by the browser.
    let version = quote_query(&theme.updated_at);
    let preview_url = if theme.preview_path.is_empty() {
        String::new()
    } else {
        format!(
            "/account/api/preview?path={}&v={version}",
            quote_query(&theme.preview_path)
        )
    };
    let screenshot_urls: Vec<String> = theme
        .screenshot_paths
        .iter()
        .map(|path| format!("/account/api/preview?path={}&v={version}", quote_query(path)))
        .collect();
    let avatar_url = theme
        .avatar_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(|path| format!("/account/api/preview?path={}&v={version}", quote_query(path)))
        .unwrap_or_default();
    json!({
        "id": theme.id,
        "version": theme.version,
        "formatVersion": theme.format_version,
        "minManagerVersion": theme.min_manager_version,
        "name": theme.name,
        "author": theme.author,
        "authorHash": theme.author_hash,
        "description": theme.description,
        "tags": theme.tags,
        "adaptive": theme.adaptive || crate::catalog::tags_mark_adaptive(&theme.tags),
        "r18": theme.r18 || crate::catalog::tags_mark_r18(&theme.tags),
        "license": theme.license,
        "size": theme.size,
        "sha256": theme.sha256,
        "updatedAt": theme.updated_at,
        "changelog": theme.changelog,
        "previewPath": theme.preview_path,
        "avatarPath": theme.avatar_path,
        "screenshotPaths": theme.screenshot_paths,
        "previewUrl": preview_url,
        "avatarUrl": avatar_url,
        "screenshotUrls": screenshot_urls,
        "downloads": theme_stats.downloads,
        "heat": theme_stats.heat,
        "downloadUrl": format!("/account/api/themes/{}/download", theme.id),
    })
}

/// Five-at-a-time discovery cards for the user center. `exclude` lets the
/// "随便看看" button avoid repeating themes until the pool is exhausted.
async fn discover_themes(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<DiscoverQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(5).clamp(1, 20);
    let Some(catalog) = crate::catalog::read_catalog(&state.paths) else {
        return Ok(Json(json!({
            "ok": true,
            "themes": [],
            "total": 0,
            "limit": limit,
            "message": "主题目录尚未生成",
        })));
    };
    let stats = state.theme_stats();
    let show_r18 = user_show_r18(&state, &session.user_id).await?;
    let include_own = query.include_own.as_deref() == Some("1");
    let sort_hot = query.sort.as_deref() == Some("hot");
    let owned = owned_theme_ids(&state, &session).await?;
    let author_filter = query
        .author
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let pool: Vec<&crate::models::CatalogEntry> = catalog
        .themes
        .iter()
        .filter(|theme| include_own || !owned.contains(&theme.id))
        .filter(|theme| show_r18 || !(theme.r18 || crate::catalog::tags_mark_r18(&theme.tags)))
        .filter(|theme| {
            author_filter
                .map(|author| theme.author.trim().eq_ignore_ascii_case(author))
                .unwrap_or(true)
        })
        .collect();
    let total = pool.len();
    let excluded: HashSet<String> = query
        .exclude
        .as_deref()
        .unwrap_or("")
        .split(',')
        .take(200)
        .map(str::trim)
        .filter(|value| valid_theme_id(value))
        .map(str::to_string)
        .collect();
    let mut candidates: Vec<&crate::models::CatalogEntry> = pool
        .iter()
        .copied()
        .filter(|theme| !excluded.contains(&theme.id))
        .collect();
    if candidates.len() < limit {
        // Everything has been seen already; recycle the full pool.
        candidates = pool;
    }
    if sort_hot {
        candidates.sort_by(|left, right| {
            let left_heat = stats.get(&left.id).map(|item| item.heat).unwrap_or(0);
            let right_heat = stats.get(&right.id).map(|item| item.heat).unwrap_or(0);
            right_heat
                .cmp(&left_heat)
                .then_with(|| left.name.cmp(&right.name))
        });
    } else {
        candidates.shuffle(&mut rand::thread_rng());
    }
    let themes: Vec<Value> = candidates
        .into_iter()
        .take(limit)
        .map(|theme| discover_theme_json(theme, stats.as_ref()))
        .collect();
    Ok(Json(json!({
        "ok": true,
        "themes": themes,
        "total": total,
        "limit": limit,
        "author": author_filter,
    })))
}

async fn download_published_theme(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Path(id): Path<String>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    let index = state.meta_index().await;
    let Some(meta) = index.theme_by_id(&id) else {
        return Err(ApiError::not_found("主题不存在"));
    };
    if meta.withdrawn_at.is_some() {
        return Err(ApiError::not_found("主题不存在"));
    }
    if method == axum::http::Method::GET {
        let _ = crate::community::record_theme_history(
            &state.db,
            &session.user_id,
            &meta.id,
            "download",
        )
        .await;
    }
    if !headers.contains_key("Range") {
        if let Some(response) = crate::distribution::redirect_asset(
            &state,
            &meta.package,
            method == axum::http::Method::GET,
        )
        .await?
        {
            return Ok(response);
        }
    }
    let object = state
        .store
        .serve(&meta.package, None)
        .await
        .map_err(|_| ApiError::not_found("主题包不存在"))?;
    if method == axum::http::Method::GET {
        state.record_theme_download(&meta.id);
    }
    let filename = format!("{}-v{}.sukisu", meta.id, meta.version);
    let mut response = crate::api::serve_object_response(
        object,
        "private, no-store",
        state.download_buffer_bytes(),
    )?;
    let disposition =
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(ApiError::internal)?;
    response
        .headers_mut()
        .insert("content-disposition", disposition);
    Ok(response)
}

async fn my_themes(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let profile_authors = authors::authors_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let hashes: HashSet<String> = profile_authors
        .into_iter()
        .map(|author| author.hash)
        .collect();
    let username = session.username.trim().to_string();
    let stats = state.theme_stats();
    let index = state.meta_index().await;
    let owned = index.owned_ids(&hashes, &username);
    let mut themes = Vec::new();
    for id in owned {
        let Some(meta) = index.theme_by_id(&id) else {
            continue;
        };
        let withdrawn_at = meta.withdrawn_at.clone();
        let expires_at = withdrawn_at
            .as_deref()
            .and_then(crate::catalog::withdrawal_expires_at);
        let theme_stats = stats.get(&meta.id).copied().unwrap_or_default();
        themes.push(json!({
            "id": &meta.id,
            "version": meta.version,
            "name": &meta.name,
            "author": &meta.author,
            "description": &meta.description,
            "tags": &meta.tags,
            "adaptive": meta.adaptive || crate::catalog::tags_mark_adaptive(&meta.tags),
            "license": &meta.license,
            "minManagerVersion": meta.min_manager_version,
            "changelog": &meta.changelog,
            "sha256": meta.sha256.clone().unwrap_or_default(),
            "updatedAt": &meta.updated_at,
            "previewPath": &meta.preview,
            "avatarPath": &meta.avatar,
            "screenshotPaths": &meta.screenshots,
            "status": if withdrawn_at.is_some() { "withdrawn" } else { "published" },
            "withdrawnAt": withdrawn_at,
            "expiresAt": expires_at,
            "downloads": theme_stats.downloads,
            "heat": theme_stats.heat,
        }));
    }
    themes.sort_by(|left, right| {
        right
            .get("updatedAt")
            .and_then(|value| value.as_str())
            .cmp(&left.get("updatedAt").and_then(|value| value.as_str()))
    });
    Ok(Json(json!({"ok": true, "themes": themes})))
}

pub(crate) async fn theme_owned_by_user(
    state: &AppState,
    session: &SessionInfo,
    meta: &crate::models::Meta,
) -> ApiResult<bool> {
    if session.role == "admin" {
        return Ok(true);
    }
    if let Some(hash) = meta
        .author_hash
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let profile_authors = authors::authors_for_user(&state.db, &session.user_id)
            .await
            .map_err(ApiError::internal)?;
        return Ok(profile_authors.iter().any(|author| author.hash == hash));
    }
    Ok(!session.username.trim().is_empty()
        && meta
            .author
            .trim()
            .eq_ignore_ascii_case(session.username.trim()))
}

async fn download_my_theme(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Path(id): Path<String>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !crate::util::valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    let index = state.meta_index().await;
    let Some(meta) = index.theme_by_id(&id) else {
        return Err(ApiError::not_found("主题不存在"));
    };
    if !theme_owned_by_user(&state, &session, meta).await? {
        return Err(ApiError::forbidden("无权下载该主题"));
    }
    if method == axum::http::Method::GET {
        let _ = crate::community::record_theme_history(
            &state.db,
            &session.user_id,
            &meta.id,
            "download",
        )
        .await;
    }
    if !headers.contains_key("Range") {
        if let Some(response) = crate::distribution::redirect_asset(
            &state,
            &meta.package,
            method == axum::http::Method::GET,
        )
        .await?
        {
            return Ok(response);
        }
    }
    let object = state
        .store
        .serve(&meta.package, None)
        .await
        .map_err(|_| ApiError::not_found("主题包不存在"))?;
    if method == axum::http::Method::GET {
        state.record_theme_download(&meta.id);
    }
    let filename = format!("{}-v{}.sukisu", meta.id, meta.version);
    let mut response = crate::api::serve_object_response(
        object,
        "private, no-store",
        state.download_buffer_bytes(),
    )?;
    let disposition =
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(ApiError::internal)?;
    response
        .headers_mut()
        .insert("content-disposition", disposition);
    Ok(response)
}

async fn withdraw_my_theme(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !crate::util::valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let lock = crate::catalog::theme_mutation_lock(&id);
    let _guard = lock.lock().await;
    let Some((meta_file, meta)) = crate::catalog::find_theme_meta(&state.paths, &id) else {
        return Err(ApiError::not_found("主题不存在"));
    };
    if !theme_owned_by_user(&state, &session, &meta).await? {
        return Err(ApiError::forbidden("无权撤回该主题"));
    }
    if let Some(withdrawn_at) = meta.withdrawn_at.as_deref() {
        return Ok(Json(json!({
            "ok": true,
            "message": "主题已撤回",
            "withdrawnAt": withdrawn_at,
            "expiresAt": crate::catalog::withdrawal_expires_at(withdrawn_at),
        })));
    }
    let withdrawn_at = crate::util::now_iso();
    let mut updated = meta;
    updated.withdrawn_at = Some(withdrawn_at.clone());
    updated.updated_at = withdrawn_at.clone();
    crate::catalog::write_meta(&meta_file, &updated).map_err(ApiError::internal)?;
    state.refresh_withdrawn_assets();
    crate::catalog::rebuild_catalog(&state.store, &state.paths)
        .await
        .map_err(ApiError::internal)?;
    accounts::withdraw_theme_submissions(&state.db, &session.user_id, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "theme.withdraw.self",
        Some("theme"),
        Some(&id),
        None,
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "message": "主题已撤回，14 天内可以重新发布",
        "withdrawnAt": withdrawn_at,
        "expiresAt": crate::catalog::withdrawal_expires_at(&withdrawn_at),
    })))
}

async fn restore_my_theme(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !crate::util::valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let lock = crate::catalog::theme_mutation_lock(&id);
    let guard = lock.lock().await;
    let Some((meta_file, meta)) = crate::catalog::find_theme_meta(&state.paths, &id) else {
        return Err(ApiError::not_found("主题不存在"));
    };
    if !theme_owned_by_user(&state, &session, &meta).await? {
        return Err(ApiError::forbidden("无权恢复该主题"));
    }
    let Some(withdrawn_at) = meta.withdrawn_at.as_deref() else {
        return Err(ApiError::bad("该主题未撤回"));
    };
    if crate::catalog::withdrawal_expired(withdrawn_at) {
        drop(guard);
        if let Ok(ids) = crate::catalog::purge_expired_withdrawn(&state.store, &state.paths).await {
            for id in ids {
                if let Err(error) = crate::community::remove_theme_activity(&state.db, &id).await {
                    tracing::warn!(theme_id = %id, "failed to clean expired theme activity: {error:#}");
                }
            }
        }
        return Err(ApiError::new(
            StatusCode::GONE,
            "该主题已超过 14 天保留期，无法恢复",
        ));
    }
    let mut updated = meta;
    updated.withdrawn_at = None;
    updated.updated_at = crate::util::now_iso();
    crate::catalog::write_meta(&meta_file, &updated).map_err(ApiError::internal)?;
    state.refresh_withdrawn_assets();
    crate::catalog::rebuild_catalog(&state.store, &state.paths)
        .await
        .map_err(ApiError::internal)?;
    accounts::restore_theme_submissions(&state.db, &session.user_id, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "theme.restore.self",
        Some("theme"),
        Some(&id),
        None,
    )
    .await;
    Ok(Json(json!({"ok": true, "message": "主题已重新发布"})))
}

async fn delete_my_theme(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !crate::util::valid_theme_id(&id) {
        return Err(ApiError::bad("无效的主题 ID"));
    }
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let Some((meta_file, meta)) = crate::catalog::find_theme_meta(&state.paths, &id) else {
        return Err(ApiError::not_found("主题不存在"));
    };
    if !theme_owned_by_user(&state, &session, &meta).await? {
        return Err(ApiError::forbidden("无权删除该主题"));
    }
    if meta.withdrawn_at.is_none() {
        return Err(ApiError::bad("只有已撤回主题可以永久删除"));
    }
    crate::catalog::remove_theme_files(&state.store, &state.paths, Some(&meta_file), &meta)
        .await
        .map_err(ApiError::internal)?;
    let _ = crate::comments::clear_theme(&state.db, &id).await;
    if let Err(error) = crate::community::remove_theme_activity(&state.db, &id).await {
        tracing::warn!(?error, "failed to clean theme community activity");
    }
    crate::catalog::rebuild_catalog(&state.store, &state.paths)
        .await
        .map_err(ApiError::internal)?;
    state.refresh_withdrawn_assets();
    accounts::delete_theme_submissions(&state.db, &session.user_id, &id)
        .await
        .map_err(ApiError::internal)?;
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "theme.delete.self",
        Some("theme"),
        Some(&id),
        None,
    )
    .await;
    Ok(Json(json!({"ok": true, "message": "主题已永久删除"})))
}

async fn my_submissions(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    let mut items = accounts::user_submissions(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    for item in &mut items {
        let status = item
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let theme_id = item
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if status == "pending" {
            let pending_id = item
                .get("submissionId")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            if let Some(id) = pending_id {
                if let Some(directory) = crate::catalog::pending_dir(&state.paths, &id) {
                    if let Some(meta) = crate::catalog::pending_meta(&state.paths, &directory) {
                        if let Some(object) = item.as_object_mut() {
                            object.insert(
                                "previewPath".to_string(),
                                json!(format!("pending/{}/{}", id, meta.preview)),
                            );
                            object.insert(
                                "avatarPath".to_string(),
                                json!(meta.avatar.map(|value| format!("pending/{}/{}", id, value))),
                            );
                            object.insert("sha256".to_string(), json!(meta.sha256));
                        }
                    }
                }
            }
        } else if status == "approved" && !theme_id.is_empty() {
            if let Some((_, meta)) = crate::catalog::find_theme_meta(&state.paths, &theme_id) {
                if let Some(object) = item.as_object_mut() {
                    object.insert("sha256".to_string(), json!(meta.sha256.unwrap_or_default()));
                }
            }
        }
    }
    Ok(Json(json!({"ok": true, "submissions": items})))
}

async fn withdraw_submission(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value
        .get("submissionId")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let owned = accounts::submission_owned(&state.db, &id, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let Some(status) = owned else {
        return Err(ApiError::not_found("投稿不存在"));
    };
    if status != "pending" {
        return Err(ApiError::bad("只有待审核投稿可以撤回"));
    }
    if let Some(directory) = crate::catalog::pending_dir(&state.paths, &id) {
        let _ = tokio::fs::remove_dir_all(directory).await;
    }
    accounts::finish_submission(&state.db, &id, "withdrawn", Some(&session.user_id), None)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "message": "投稿已撤回"})))
}

async fn delete_submission(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let id = value
        .get("submissionId")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let owned = accounts::submission_owned(&state.db, &id, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let Some(status) = owned else {
        return Err(ApiError::not_found("投稿不存在"));
    };
    if status != "withdrawn" && status != "rejected" {
        return Err(ApiError::bad("只有已撤回或已拒绝的投稿记录可以删除"));
    }
    accounts::delete_submission(&state.db, &id, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    let _ = accounts::audit(
        &state.db,
        Some(&session.user_id),
        "submission.delete.self",
        Some("submission"),
        Some(&id),
        None,
    )
    .await;
    Ok(Json(json!({"ok": true, "message": "投稿记录已删除"})))
}

#[derive(Deserialize)]
struct PreviewQuery {
    path: String,
    /// Asset version. Versioned keys may be cached for a long time; unversioned
    /// requests (catalog URLs, rotated edge routes) must be revalidated so a
    /// replaced preview shows up immediately.
    #[serde(default)]
    v: Option<String>,
}

async fn account_preview(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Query(query): Query<PreviewQuery>,
) -> ApiResult<Response> {
    let path = query.path.trim().trim_start_matches('/').to_string();
    if path.is_empty() || path.contains("..") || path.contains('\\') {
        return Err(ApiError::bad("无效的预览路径"));
    }
    let versioned = query.v.as_deref().is_some_and(|v| !v.trim().is_empty());
    let published_ttl = if versioned {
        "private, max-age=604800, immutable"
    } else {
        "private, no-cache"
    };
    let mut cache_control = "private, no-store";
    let allowed = if path.starts_with("avatars/users/") {
        path.starts_with(&format!("avatars/users/{}", session.user_id)) || session.role == "admin"
    } else if let Some(rest) = path.strip_prefix("pending/") {
        let submission = rest.split('/').next().unwrap_or("");
        accounts::submission_owned(&state.db, submission, &session.user_id)
            .await
            .map_err(ApiError::internal)?
            .is_some()
            || session.role == "admin"
    } else if path.starts_with("avatars/authors/") {
        // Author avatars are public store assets. They can be replaced in place,
        // so an unversioned request is revalidated on every navigation.
        cache_control = published_ttl;
        true
    } else {
        // Published preview/screenshot assets are public to signed-in users.
        // Only withdrawn assets need the more expensive ownership lookup.
        let index = state.meta_index().await;
        let published = index
            .theme_for_asset(&path)
            .is_some_and(|meta| meta.withdrawn_at.is_none());
        let owned = if published {
            false
        } else if let Some(meta) = index.theme_for_asset(&path) {
            let profile_authors = authors::authors_for_user(&state.db, &session.user_id)
                .await
                .map_err(ApiError::internal)?;
            let hashes: HashSet<String> = profile_authors
                .into_iter()
                .map(|author| author.hash)
                .collect();
            index
                .owned_ids(&hashes, &session.username)
                .contains(&meta.id)
        } else {
            false
        };
        if published {
            cache_control = published_ttl;
        }
        published || owned || session.role == "admin"
    };
    if !allowed {
        return Err(ApiError::forbidden("无权查看该资源"));
    }
    if path.starts_with("pending/") {
        let target = crate::util::safe_join(&state.paths.root, &path)
            .ok_or_else(|| ApiError::bad("无效的预览路径"))?;
        let data = tokio::fs::read(target)
            .await
            .map_err(|_| ApiError::not_found("资源不存在"))?;
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type_for(&path))
            .header("Cache-Control", "private, no-store")
            .header("Content-Length", data.len().to_string())
            .body(axum::body::Body::from(data))
            .map_err(ApiError::internal);
    }
    if let Some(response) = crate::distribution::redirect_asset(&state, &path, false).await? {
        return Ok(response);
    }
    let object = state
        .store
        .serve(&path, None)
        .await
        .map_err(|_| ApiError::not_found("资源不存在"))?;
    crate::api::serve_object_response(object, cache_control, state.download_buffer_bytes())
}

struct ResolvedAuthor {
    author: authors::Author,
    generated_id: Option<String>,
    requires_review: bool,
}

async fn ensure_author_request(
    state: &AppState,
    user_id: &str,
    kind: &str,
    hash: &str,
    display_name: &str,
) -> ApiResult<()> {
    let pending = authors::author_requests(&state.db, Some("pending"), Some(user_id))
        .await
        .map_err(ApiError::internal)?;
    if pending
        .iter()
        .any(|item| item.get("authorHash").and_then(|value| value.as_str()) == Some(hash))
    {
        return Ok(());
    }
    authors::create_author_request(
        &state.db,
        user_id,
        kind,
        Some(hash),
        display_name,
        Some("由投稿自动创建"),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(())
}

async fn resolve_account_author(
    state: &AppState,
    session: &SessionInfo,
    author_id: Option<&str>,
    display_name: &str,
) -> ApiResult<ResolvedAuthor> {
    let display_name = if display_name.trim().is_empty() {
        session.display_name.clone()
    } else {
        display_name.trim().to_string()
    };
    if let Some(author) = authors::active_author_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
    {
        return Ok(ResolvedAuthor {
            author,
            generated_id: None,
            requires_review: false,
        });
    }
    if let Some(author_id) = author_id.map(str::trim).filter(|value| !value.is_empty()) {
        if !valid_author_id(author_id) {
            return Err(ApiError::bad("作者 ID 需要 16-128 位字母、数字或 . _ ~ -"));
        }
        let hash = author_key(author_id);
        let fingerprint = author_fingerprint(&hash);
        if let Some(mut author) = authors::author_by_hash(&state.db, &hash)
            .await
            .map_err(ApiError::internal)?
        {
            if let Some(owner) = author.user_id.as_deref() {
                if owner != session.user_id {
                    return Err(ApiError::conflict("该作者 ID 已绑定其他账号"));
                }
            }
            if author.status == "retired" || author.status == "revoked" {
                return Err(ApiError::bad("该作者 ID 已停用，请申请新的作者身份"));
            }
            if author.display_name.as_deref() != Some(display_name.as_str()) {
                let _ = authors::update_author_name(&state.db, &hash, &display_name).await;
            }
            let requires_review = author.user_id.is_none() || author.status != "active";
            if author.user_id.is_none() {
                if authors::has_pending_request(&state.db, &session.user_id)
                    .await
                    .map_err(ApiError::internal)?
                {
                    return Err(ApiError::conflict("你还有待审核的作者申请，请等待审核完成"));
                }
                ensure_author_request(state, &session.user_id, "bind", &hash, &display_name)
                    .await?;
            }
            author.user_id = Some(session.user_id.clone());
            return Ok(ResolvedAuthor {
                author,
                generated_id: None,
                requires_review,
            });
        }
        if authors::has_pending_request(&state.db, &session.user_id)
            .await
            .map_err(ApiError::internal)?
        {
            return Err(ApiError::conflict("你还有待审核的作者申请，请等待审核完成"));
        }
        let author = authors::create_author(
            &state.db,
            &hash,
            &fingerprint,
            &display_name,
            Some(&session.user_id),
            "pending",
        )
        .await
        .map_err(ApiError::internal)?;
        let _ = authors::set_auto_publish(&state.db, &hash, false).await;
        ensure_author_request(state, &session.user_id, "new", &hash, &display_name).await?;
        return Ok(ResolvedAuthor {
            author,
            generated_id: None,
            requires_review: true,
        });
    }

    if let Some(author) = authors::active_author_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
    {
        return Ok(ResolvedAuthor {
            author,
            generated_id: None,
            requires_review: false,
        });
    }
    let existing = authors::authors_for_user(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?;
    if let Some(author) = existing
        .into_iter()
        .find(|author| author.status == "pending")
    {
        return Ok(ResolvedAuthor {
            author,
            generated_id: None,
            requires_review: true,
        });
    }
    if authors::has_pending_request(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict("你还有待审核的作者申请，请等待审核完成"));
    }
    let raw_id = random_hex(24);
    let hash = author_key(&raw_id);
    let author = authors::create_author(
        &state.db,
        &hash,
        &author_fingerprint(&hash),
        &display_name,
        Some(&session.user_id),
        "pending",
    )
    .await
    .map_err(ApiError::internal)?;
    let _ = authors::set_auto_publish(&state.db, &hash, false).await;
    ensure_author_request(state, &session.user_id, "new", &hash, &display_name).await?;
    Ok(ResolvedAuthor {
        author,
        generated_id: Some(raw_id),
        requires_review: true,
    })
}

async fn next_theme_id(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
) -> ApiResult<Json<Value>> {
    require_active(&session)?;
    let resolved = resolve_account_author(&state, &session, None, &session.display_name).await?;
    if authors::has_restriction(&state.db, &resolved.author.hash, "upload")
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::forbidden("该作者身份已被限制投稿"));
    }
    let id = authors::peek_next_theme_id(&state.db, &resolved.author.hash)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::conflict("无法分配主题 ID"))?;
    let author = authors::author_by_hash(&state.db, &resolved.author.hash)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or(resolved.author);
    Ok(Json(json!({
        "ok": true,
        "id": id,
        "author": author,
    })))
}

async fn submit_theme(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Response> {
    require_active(&session)?;
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let (pending_count, _) = crate::catalog::pending_state(&state.paths);
    if pending_count >= crate::config::MAX_PENDING_SUBMISSIONS {
        return Err(ApiError::too_many("待审核队列已满，请稍后再试"));
    }
    let _upload_permit = state
        .upload_semaphore
        .acquire()
        .await
        .map_err(|_| ApiError::internal("上传队列不可用"))?;
    let _live_upload = state.performance.live.upload_guard();
    let mut form = crate::uploads::parse_multipart(multipart)
        .await
        .map_err(ApiError::bad)?;
    let upload_ids = crate::resumable::resolve_form(&state, &session.user_id, &mut form).await?;
    let author_id = form.author_id.clone();
    let author_name = form.author.clone();
    let requested_id = form.id.trim().to_string();
    let resolved =
        resolve_account_author(&state, &session, author_id.as_deref(), &author_name).await?;
    if authors::has_restriction(&state.db, &resolved.author.hash, "upload")
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::forbidden("该作者身份已被限制投稿"));
    }
    crate::uploads::reuse_author_avatar(
        &state.store,
        &state.paths,
        &resolved.author.hash,
        resolved.author.display_name.as_deref(),
        &mut form.avatar,
    )
    .await;
    let theme_id = if requested_id.is_empty() {
        crate::authors::allocate_unique_theme_id(&state.db, &state.paths, &resolved.author.hash)
            .await
            .map_err(ApiError::internal)?
    } else {
        requested_id
    };
    form.id = theme_id.clone();
    let prepared = crate::uploads::prepare_upload(form)
        .await
        .map_err(ApiError::bad)?;
    let prepared_version = prepared.version as i64;
    let prepared_name = prepared.name.clone();
    let package_sha256 = prepared.package_sha256.clone();
    let old_meta = crate::catalog::find_theme_meta(&state.paths, &theme_id);
    let author_hash = resolved.author.hash.clone();
    if let Some((_, old)) = &old_meta {
        let legacy_owner = old
            .author_hash
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true);
        if legacy_owner {
            let same_owner = !session.username.trim().is_empty()
                && old
                    .author
                    .trim()
                    .eq_ignore_ascii_case(session.username.trim());
            if !same_owner {
                return Err(ApiError::conflict("该主题属于其他作者"));
            }
        } else if let Some(owner) = old.author_hash.as_deref() {
            if owner != author_hash {
                let old_author = authors::author_by_hash(&state.db, owner)
                    .await
                    .map_err(ApiError::internal)?;
                let owned_by_user = old_author
                    .as_ref()
                    .and_then(|author| author.user_id.as_deref())
                    == Some(session.user_id.as_str());
                if !owned_by_user {
                    return Err(ApiError::conflict("该主题 ID 属于其他作者"));
                }
            }
        }
        if old.version >= prepared.version as i64 {
            return Err(ApiError::conflict(
                "该主题已有同版本或更高版本，请提升版本号",
            ));
        }
    }
    if crate::catalog::pending_submissions(&state.paths)
        .iter()
        .any(|item| item.get("id").and_then(|value| value.as_str()) == Some(theme_id.as_str()))
    {
        return Err(ApiError::conflict("该主题已在审核队列中"));
    }
    let new_size = prepared.new_size();
    let limit = state.limit_bytes.load(std::sync::atomic::Ordering::Relaxed);
    if let Err(error) = state
        .store
        .choose_location(new_size, &state.paths, limit)
        .await
    {
        return Err(ApiError::bad(error.to_string()));
    }
    if let Some(avatar) = &prepared.avatar {
        let _ = crate::uploads::persist_author_avatar(&state.store, &author_hash, avatar).await;
    }
    let auto_publish = !resolved.requires_review
        && resolved.author.status == "active"
        && resolved.author.auto_publish
        && !authors::has_restriction(&state.db, &resolved.author.hash, "auto_publish")
            .await
            .map_err(ApiError::internal)?;
    if auto_publish {
        let themes = crate::uploads::publish_theme(
            &state.store,
            &state.settings,
            &state.paths,
            limit,
            prepared,
            Some(author_hash.clone()),
            0,
            old_meta,
        )
        .await?;
        state.refresh_withdrawn_assets();
        if let Err(error) = crate::community::record_release(
            &state.db,
            &author_hash,
            &theme_id,
            &prepared_name,
            prepared_version,
        )
        .await
        {
            tracing::warn!(?error, "failed to record theme update");
        }
        let _ = authors::touch_author(&state.db, &author_hash).await;
        let submission_id = crate::util::submission_id();
        accounts::record_submission(
            &state.db,
            &submission_id,
            &session.user_id,
            &theme_id,
            &prepared_name,
            prepared_version,
            "approved",
            true,
        )
        .await
        .map_err(ApiError::internal)?;
        accounts::audit(
            &state.db,
            Some(&session.user_id),
            "theme.submit.auto",
            Some("theme"),
            Some(&theme_id),
            None,
        )
        .await
        .ok();
        for id in &upload_ids {
            crate::resumable::remove_session(&state, &session.user_id, id).await;
        }
        return Ok(Json(json!({
            "ok": true,
            "pending": false,
            "autoApproved": true,
            "id": theme_id,
            "themes": themes.len(),
            "sha256": package_sha256.clone(),
            "authorStatus": resolved.author.status,
            "authorId": resolved.generated_id,
            "message": "作者许可有效，已自动发布",
        }))
        .into_response());
    }

    let submission = crate::util::submission_id();
    let remote = request_ip(&headers);
    {
        // Writing a 128 MB package to the pending directory is blocking file IO.
        // Doing it inline stalled the async runtime right when the manager was
        // waiting for the verification result.
        let paths = state.paths.clone();
        let author_hash = author_hash.clone();
        let author_fingerprint = resolved.author.fingerprint.clone();
        let user_id = session.user_id.clone();
        let write_submission = submission.clone();
        tokio::task::spawn_blocking(move || {
            crate::uploads::write_pending(
                &paths,
                &prepared,
                &author_hash,
                &author_fingerprint,
                &remote,
                &write_submission,
                Some(&user_id),
            )
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    }
    accounts::record_submission(
        &state.db,
        &submission,
        &session.user_id,
        &theme_id,
        &prepared_name,
        prepared_version,
        "pending",
        false,
    )
    .await
    .map_err(ApiError::internal)?;
    for id in &upload_ids {
        crate::resumable::remove_session(&state, &session.user_id, id).await;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "pending": true,
            "autoApproved": false,
            "id": theme_id,
            "submissionId": submission,
            "sha256": package_sha256,
            "authorStatus": resolved.author.status,
            "authorId": resolved.generated_id,
            "message": "投稿已提交，等待管理员审核",
        })),
    )
        .into_response())
}
