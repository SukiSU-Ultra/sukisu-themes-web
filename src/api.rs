use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Multipart, OriginalUri, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use crate::auth;
use crate::catalog;
use crate::config::{
    self, read_env, set_env_key, validate_s3_settings, validate_storage_root, write_storage_config,
    Backend, Paths, StorageSettings, CSRF_HEADER, CSRF_VALUE, CURRENT_FORMAT_VERSION,
    MAX_BODY_BYTES, MAX_PENDING_SUBMISSIONS, MAX_SUBMISSIONS_PER_HOUR, MIN_FORMAT_VERSION,
    TOKEN_HEADER,
};
use crate::jobs::JobManager;
use crate::storage::{self, AssetStore, ServeBody, ServeError};
use crate::system;
use crate::uploads::{self, PublishError};
use crate::util::{
    image_extension_allowed, random_hex, valid_author_hash, valid_theme_id, valid_token,
};

const EMBEDDED_INDEX_HTML: &str = include_str!("../static/index.html");
const EMBEDDED_LOGIN_HTML: &str = include_str!("../static/login.html");
const EMBEDDED_SETUP_HTML: &str = include_str!("../static/setup.html");
const EMBEDDED_ACCOUNT_AUTH_HTML: &str = include_str!("../static/account-auth.html");
const EMBEDDED_ACCOUNT_HTML: &str = include_str!("../static/account.html");
const EMBEDDED_LOGO_SVG: &[u8] = include_bytes!("../static/logo.svg");
const EMBEDDED_I18N_JS: &str = include_str!("../static/i18n.js");
const EMBEDDED_LOCALE_EN: &str = include_str!("../static/locales/en.json");

#[derive(Clone)]
pub struct AppState {
    pub paths: Arc<Paths>,
    pub settings: Arc<StorageSettings>,
    pub limit_bytes: Arc<AtomicU64>,
    pub store: AssetStore,
    pub http: reqwest::Client,
    pub jobs: JobManager,
    pub rate: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    pub account_rate: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    pub db: crate::db::Database,
    pub sessions: auth::SessionStore,
    pub sources: Arc<crate::sources::SourcesStore>,
    pub login_failures: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    withdrawn_assets: Arc<RwLock<HashSet<String>>>,
}

impl AppState {
    pub async fn new(settings: StorageSettings) -> anyhow::Result<Self> {
        let paths = Arc::new(config::load_paths(&settings));
        config::ensure_storage_dirs(&paths, &settings)?;
        let primary = storage::build_store(&settings)?;
        let db = crate::db::Database::open(&paths.db).await?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        let sources = crate::sources::SourcesStore::load(paths.sources_file.clone()).await;
        let location_specs = crate::sources::location_specs(&sources).await;
        let store = storage::build_multi(primary, location_specs, http.clone());
        let limit_bytes = settings.limit_bytes;
        let sessions = auth::SessionStore::new(db.clone());
        let withdrawn_assets = Arc::new(RwLock::new(catalog::withdrawn_asset_keys(&paths)));
        Ok(Self {
            paths,
            settings: Arc::new(settings),
            limit_bytes: Arc::new(AtomicU64::new(limit_bytes)),
            store,
            http,
            jobs: JobManager::new(),
            rate: Arc::new(Mutex::new(HashMap::new())),
            account_rate: Arc::new(Mutex::new(HashMap::new())),
            db,
            sessions,
            sources,
            login_failures: Arc::new(Mutex::new(HashMap::new())),
            withdrawn_assets,
        })
    }

    pub fn refresh_withdrawn_assets(&self) {
        let keys = catalog::withdrawn_asset_keys(&self.paths);
        if let Ok(mut guard) = self.withdrawn_assets.write() {
            *guard = keys;
        }
    }

    pub fn is_withdrawn_asset(&self, path: &str) -> bool {
        self.withdrawn_assets
            .read()
            .map(|guard| guard.contains(path))
            .unwrap_or(false)
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub(crate) fn too_many(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"ok": false, "error": self.message})),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self::internal(value)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(value: std::io::Error) -> Self {
        Self::internal(value)
    }
}

impl From<PublishError> for ApiError {
    fn from(value: PublishError) -> Self {
        match value {
            PublishError::Bad(message) => Self::bad(message),
            PublishError::Conflict(message) => Self::conflict(message),
            PublishError::Internal(error) => Self::internal(error),
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/session", get(session_info))
        .route("/setup", post(setup))
        .route("/status", get(status))
        .route("/themes", get(themes))
        .route("/pending", get(pending))
        .route("/authors", get(authors))
        .route("/preview", get(preview))
        .route("/storage", get(storage_get))
        .route("/storage/job", get(storage_job))
        .route("/upload-theme", post(upload_theme))
        .route("/token", post(set_token))
        .route("/token/generate", post(generate_token))
        .route("/password", post(set_password))
        .route("/delete-theme", post(delete_theme))
        .route("/approve", post(approve))
        .route("/reject", post(reject))
        .route("/author/revoke", post(author_revoke))
        .route("/author/delete", post(author_delete))
        .route("/sync", post(sync_catalog))
        .route("/test-catalog", post(test_catalog))
        .route("/reload", post(reload_caddy))
        .route("/storage/test", post(storage_test))
        .route("/storage/root", post(storage_root))
        .route("/storage/backend", post(storage_backend))
        .route("/storage/limit", post(storage_limit))
        .merge(crate::admin_users::router())
        .merge(crate::network_api::router())
        .merge(crate::auth_settings_api::router())
        .merge(crate::sources_api::router())
        .merge(crate::resumable::router())
        .layer(middleware::from_fn_with_state(state.clone(), auth_guard))
        .layer(middleware::from_fn(csrf_guard));

    let account_api = crate::account_api::router(state.clone());

    Router::new()
        .nest("/api", api.clone())
        .nest("/admin/api", api)
        .nest("/account/api", account_api)
        .route("/submit", post(submit))
        .route("/themes/{*path}", get(asset))
        .route("/previews/{*path}", get(asset))
        .route("/screenshots/{*path}", get(asset))
        .route("/avatars/{*path}", get(asset))
        .fallback(get(static_file))
        .layer(DefaultBodyLimit::max(config::MAX_UPLOAD_BODY_BYTES))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn csrf_guard(request: Request, next: Next) -> Response {
    if request.method() == Method::POST {
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

async fn auth_guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let relative = path
        .strip_prefix("/admin/api")
        .or_else(|| path.strip_prefix("/api"))
        .unwrap_or(&path);
    if matches!(relative, "/login" | "/session" | "/logout" | "/setup") {
        return next.run(request).await;
    }
    let Some(token) = auth::session_token(request.headers(), auth::ADMIN_SESSION_COOKIE) else {
        return ApiError::unauthorized("未登录").into_response();
    };
    let Some(session) = state.sessions.validate(&token, "admin").await else {
        return ApiError::unauthorized("登录已过期，请重新登录").into_response();
    };
    request.extensions_mut().insert(session);
    next.run(request).await
}

fn login_allowed(state: &AppState, ip: &str) -> bool {
    let mut failures = state.login_failures.lock().unwrap();
    let now = Instant::now();
    let list = failures.entry(ip.to_string()).or_default();
    list.retain(|stamp| now.duration_since(*stamp) < Duration::from_secs(300));
    list.len() < 10
}

fn record_login_failure(state: &AppState, ip: &str) {
    let mut failures = state.login_failures.lock().unwrap();
    let now = Instant::now();
    let list = failures.entry(ip.to_string()).or_default();
    list.retain(|stamp| now.duration_since(*stamp) < Duration::from_secs(300));
    list.push(now);
    if failures.len() > 1000 {
        let keys: Vec<String> = failures.keys().take(200).cloned().collect();
        for key in keys {
            failures.remove(&key);
        }
    }
}

fn clear_login_failures(state: &AppState, ip: &str) {
    state.login_failures.lock().unwrap().remove(ip);
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let username = value
        .get("username")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if username.is_empty() || password.is_empty() {
        return Err(ApiError::bad("请输入用户名和密码"));
    }
    if !auth::admin_configured(&state.db, &state.paths).await {
        return Err(ApiError::conflict("尚未完成初始化，请先打开初始化页面"));
    }
    let remote = request_ip(&headers);
    if !login_allowed(&state, &remote) {
        return Err(ApiError::too_many("尝试次数过多，请 5 分钟后再试"));
    }

    let user = crate::accounts::user_by_login(&state.db, &username)
        .await
        .map_err(ApiError::internal)?;
    let Some(user) = user else {
        record_login_failure(&state, &remote);
        return Err(ApiError::unauthorized("用户名或密码错误"));
    };
    if !user.is_admin() || !user.is_active() {
        record_login_failure(&state, &remote);
        return Err(ApiError::unauthorized("用户名或密码错误"));
    }
    let pass = password.clone();
    let hash = crate::accounts::user_password_hash(&state.db, &user.id)
        .await
        .map_err(ApiError::internal)?;
    let verified = match hash {
        Some(hash) => tokio::task::spawn_blocking(move || bcrypt::verify(pass, &hash))
            .await
            .map_err(|error| ApiError::internal(error.to_string()))?
            .map_err(|error| ApiError::internal(error.to_string()))?,
        None => false,
    };
    if !verified {
        record_login_failure(&state, &remote);
        return Err(ApiError::unauthorized("用户名或密码错误"));
    }
    clear_login_failures(&state, &remote);
    let user_agent = header_text(&headers, "user-agent");
    let token = state
        .sessions
        .create(
            &user.id,
            "admin",
            auth::SESSION_TTL_SECS,
            &user_agent,
            &remote,
        )
        .await
        .map_err(ApiError::internal)?;
    let cookie = auth::session_cookie(
        auth::ADMIN_SESSION_COOKIE,
        &token,
        "/",
        auth::SESSION_TTL_SECS,
        auth::request_is_secure(&headers),
    );
    let payload = json!({
        "ok": true,
        "username": user.username,
        "expiresIn": auth::SESSION_TTL_SECS,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Set-Cookie", cookie)
        .body(Body::from(payload.to_string()))
        .map_err(ApiError::internal)
}

async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    if auth::admin_configured(&state.db, &state.paths).await {
        return Err(ApiError::conflict("控制台已经完成初始化"));
    }
    let value = json_body(body).await?;
    let username = value
        .get("username")
        .and_then(|value| value.as_str())
        .unwrap_or("admin")
        .trim()
        .to_string();
    if !auth::valid_admin_username(&username) {
        return Err(ApiError::bad("用户名需要 3-32 位字母、数字或 . _ -"));
    }
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if password.len() < 8 {
        return Err(ApiError::bad("密码至少 8 位"));
    }
    let limit_mb = value
        .get("limitMb")
        .and_then(|value| value.as_u64())
        .unwrap_or(1024);
    let limit_bytes = limit_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| ApiError::bad("存储配额超出范围"))?;
    config::validate_storage_limit(limit_bytes)
        .map_err(|error| ApiError::bad(error.to_string()))?;

    let pass = password.clone();
    let hash = tokio::task::spawn_blocking(move || auth::hash_admin_password(&pass))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::internal)?;
    let user = crate::accounts::create_user(
        &state.db,
        crate::accounts::NewUser {
            username: &username,
            display_name: &username,
            email: None,
            password_hash: Some(&hash),
            role: "admin",
            status: "active",
            email_verified: true,
        },
    )
    .await
    .map_err(|error| ApiError::bad(error.to_string()))?;
    set_env_key(&state.paths.admin_env, "ADMIN_USERNAME", &username).map_err(ApiError::internal)?;
    set_env_key(&state.paths.admin_env, "ADMIN_PASSWORD_HASH", &hash)
        .map_err(ApiError::internal)?;
    set_env_key(
        &state.paths.storage_env,
        "THEME_STORAGE_LIMIT_BYTES",
        &limit_bytes.to_string(),
    )
    .map_err(ApiError::internal)?;
    state.limit_bytes.store(limit_bytes, Ordering::Relaxed);
    let _ = tokio::fs::remove_file(&state.paths.admin_password_file).await;

    let token = state
        .sessions
        .create(
            &user.id,
            "admin",
            auth::SESSION_TTL_SECS,
            &header_text(&headers, "user-agent"),
            &request_ip(&headers),
        )
        .await
        .map_err(ApiError::internal)?;
    let cookie = auth::session_cookie(
        auth::ADMIN_SESSION_COOKIE,
        &token,
        "/",
        auth::SESSION_TTL_SECS,
        auth::request_is_secure(&headers),
    );
    let payload = json!({
        "ok": true,
        "username": user.username,
        "expiresIn": auth::SESSION_TTL_SECS,
        "limitBytes": limit_bytes,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Set-Cookie", cookie)
        .body(Body::from(payload.to_string()))
        .map_err(ApiError::internal)
}

async fn session_info(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let token = auth::session_token(&headers, auth::ADMIN_SESSION_COOKIE);
    let session = match token.as_deref() {
        Some(token) => state.sessions.validate(token, "admin").await,
        None => None,
    };
    let admin_username = crate::accounts::admin_username_hint(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| auth::configured_username(&state.paths));
    Json(json!({
        "ok": true,
        "authenticated": session.is_some(),
        "username": session.as_ref().map(|session| session.username.clone()),
        "role": session.as_ref().map(|session| session.role.clone()),
        "adminUsername": admin_username,
        "setupRequired": !auth::admin_configured(&state.db, &state.paths).await,
    }))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::session_token(&headers, auth::ADMIN_SESSION_COOKIE) {
        state.sessions.remove(&token).await;
    }
    if let Some(token) = auth::session_token(&headers, auth::USER_SESSION_COOKIE) {
        state.sessions.remove(&token).await;
    }
    let secure = auth::request_is_secure(&headers);
    let admin_cookie = auth::clear_session_cookie(auth::ADMIN_SESSION_COOKIE, "/", secure);
    let user_cookie = auth::clear_session_cookie(auth::USER_SESSION_COOKIE, "/", secure);
    let legacy_user_cookie =
        auth::clear_session_cookie(auth::USER_SESSION_COOKIE, "/account", secure);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Set-Cookie", admin_cookie)
        .header("Set-Cookie", user_cookie)
        .header("Set-Cookie", legacy_user_cookie)
        .body(Body::from(json!({"ok": true}).to_string()))
        .unwrap_or_else(|_| ApiError::internal("logout response").into_response())
}

fn read_token(paths: &Paths) -> String {
    read_env(&paths.token_env)
        .get("THEME_STORE_TOKEN")
        .cloned()
        .unwrap_or_default()
}

fn token_matches(expected: &str, provided: &str) -> bool {
    expected.len() == provided.len() && expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

pub(crate) fn request_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("local")
        .to_string()
}

pub(crate) fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn json_body(body: Bytes) -> ApiResult<Value> {
    if body.len() > MAX_BODY_BYTES {
        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "请求体过大"));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| ApiError::bad("invalid JSON"))?;
    if !value.is_object() {
        return Err(ApiError::bad("JSON object expected"));
    }
    Ok(value)
}

fn service_state_json(unit: &str, state: &AppState) -> Value {
    let _ = state;
    json!(unit)
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    let token = read_token(&state.paths);
    let config = read_env(&state.paths.admin_config);
    let public = config
        .get("PUBLIC_URL")
        .map(|value| value.trim_end_matches('/').to_string())
        .unwrap_or_default();
    let admin = config
        .get("ADMIN_URL")
        .map(|value| value.trim_end_matches('/').to_string())
        .unwrap_or_default();
    let admin_api = if admin.is_empty() {
        String::new()
    } else {
        format!("{admin}/api")
    };
    let mut endpoints = Vec::new();
    if !public.is_empty() {
        endpoints.push(json!({"name": "主题目录", "method": "GET", "url": format!("{public}/catalog.json"), "auth": "访问令牌"}));
        endpoints.push(json!({"name": "健康检查", "method": "GET", "url": format!("{public}/health"), "auth": "公开"}));
        endpoints.push(json!({"name": "主题包下载", "method": "GET", "url": format!("{public}/themes/<id>-v<version>.sukisu"), "auth": "访问令牌"}));
        endpoints.push(json!({"name": "提交主题", "method": "POST", "url": format!("{public}/submit"), "auth": "访问令牌"}));
    }
    if !admin_api.is_empty() {
        for (name, method, path) in [
            ("管理状态", "GET", "/status"),
            ("主题列表", "GET", "/themes"),
            ("待审核列表", "GET", "/pending"),
            ("自动审批作者", "GET", "/authors"),
            ("撤销作者许可", "POST", "/author/revoke"),
            ("批准主题", "POST", "/approve"),
            ("拒绝主题", "POST", "/reject"),
            ("预览图", "GET", "/preview?path=previews/…"),
            ("修改令牌", "POST", "/token"),
            ("上传主题", "POST", "/upload-theme"),
            ("删除主题", "POST", "/delete-theme"),
            ("测试目录", "POST", "/test-catalog"),
            ("重建目录", "POST", "/sync"),
            ("修改密码", "POST", "/password"),
            ("存储状态", "GET", "/storage"),
            ("测试对象存储", "POST", "/storage/test"),
            ("修改存储目录", "POST", "/storage/root"),
            ("切换存储后端", "POST", "/storage/backend"),
            ("修改存储配额", "POST", "/storage/limit"),
            ("存储迁移进度", "GET", "/storage/job"),
            ("用户列表", "GET", "/users"),
            ("账号审核", "POST", "/users/{id}/approve"),
            ("作者申请", "GET", "/author-requests"),
            ("操作日志", "GET", "/audit"),
        ] {
            endpoints.push(json!({
                "name": name,
                "method": method,
                "url": format!("{admin_api}{path}"),
                "auth": "登录会话",
            }));
        }
    }
    let (pending_count, pending_bytes) = catalog::pending_state(&state.paths);
    let authors_count = crate::authors::list_authors(&state.db)
        .await
        .map(|items| items.len())
        .unwrap_or(0);
    let users_count = crate::accounts::count_users(&state.db).await.unwrap_or(0);
    let users_pending = crate::accounts::count_pending_users(&state.db)
        .await
        .unwrap_or(0);
    let author_requests_pending = crate::authors::count_pending_author_requests(&state.db)
        .await
        .unwrap_or(0);
    let usage = catalog::storage_usage(
        &state.store,
        &state.paths,
        state.settings.backend.as_str(),
        state.limit_bytes.load(Ordering::Relaxed),
    )
    .await;
    let storage_value = serde_json::to_value(&usage).unwrap_or_else(|_| json!({}));
    let caddy = system::service_state("caddy").await;
    let sync_timer = system::service_state("sukisu-themes-sync.timer").await;
    let admin_service = system::service_state("sukisu-theme-web.service").await;
    Json(json!({
        "tokenSet": !token.is_empty(),
        "tokenHeader": TOKEN_HEADER,
        "formatVersion": CURRENT_FORMAT_VERSION,
        "minFormatVersion": MIN_FORMAT_VERSION,
        "supportedFormats": (MIN_FORMAT_VERSION..=CURRENT_FORMAT_VERSION).collect::<Vec<_>>(),
        "catalog": catalog::catalog_state(&state.paths),
        "pendingCount": pending_count,
        "pendingBytes": pending_bytes,
        "authorsCount": authors_count,
        "usersCount": users_count,
        "usersPending": users_pending,
        "authorRequestsPending": author_requests_pending,
        "mailConfigured": crate::mailer::configured(&state.paths),
        "githubConfigured": crate::oauth::GithubSettings::load(&state.paths).is_some(),
        "managerVersionCode": crate::config::manager_version_code(&state.paths),
        "storage": storage_value,
        "storageBackend": state.settings.backend.as_str(),
        "stateDir": state.paths.root.to_string_lossy(),
        "caddy": caddy,
        "syncTimer": sync_timer,
        "adminService": admin_service,
        "publicUrl": config.get("PUBLIC_URL").cloned().unwrap_or_default(),
        "adminUrl": config.get("ADMIN_URL").cloned().unwrap_or_default(),
        "adminApiBase": admin_api,
        "catalogUrl": if public.is_empty() { String::new() } else { format!("{public}/catalog.json") },
        "healthUrl": if public.is_empty() { String::new() } else { format!("{public}/health") },
        "endpoints": endpoints,
    }))
}

async fn themes(State(state): State<AppState>) -> Json<Value> {
    let themes = catalog::admin_themes(&state.paths).await;
    let generated_at = catalog::read_catalog(&state.paths).and_then(|catalog| catalog.generated_at);
    Json(json!({"ok": true, "themes": themes, "generatedAt": generated_at}))
}

async fn pending(State(state): State<AppState>) -> Json<Value> {
    let mut submissions = catalog::pending_submissions(&state.paths);
    for item in &mut submissions {
        let user_id = item
            .get("submitterUserId")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if !user_id.is_empty() {
            if let Ok(Some(user)) = crate::accounts::user_by_id(&state.db, &user_id).await {
                if let Some(object) = item.as_object_mut() {
                    object.insert("submitterUsername".to_string(), json!(user.username));
                    object.insert("submitterDisplayName".to_string(), json!(user.display_name));
                    object.insert("submitterEmail".to_string(), json!(user.email));
                }
            }
        }
    }
    let count = submissions.len();
    Json(json!({"ok": true, "submissions": submissions, "count": count}))
}

async fn authors(State(state): State<AppState>) -> Json<Value> {
    let authors = crate::authors::list_authors(&state.db)
        .await
        .unwrap_or_default();
    let count = authors.len();
    Json(json!({"ok": true, "authors": authors, "count": count}))
}

#[derive(Deserialize)]
struct PreviewQuery {
    path: String,
}

async fn preview(
    State(state): State<AppState>,
    Query(query): Query<PreviewQuery>,
) -> ApiResult<Response> {
    let relative = query.path;
    if !relative.starts_with("previews/")
        && !relative.starts_with("screenshots/")
        && !relative.starts_with("avatars/")
        && !relative.starts_with("pending/")
    {
        return Err(ApiError::not_found("not found"));
    }
    let ext = crate::util::file_extension(&relative);
    if !image_extension_allowed(&ext) {
        return Err(ApiError::not_found("not found"));
    }
    let body = if relative.starts_with("pending/") {
        let Some(path) = crate::util::safe_join(&state.paths.root, &relative) else {
            return Err(ApiError::not_found("not found"));
        };
        tokio::fs::read(&path)
            .await
            .map_err(|_| ApiError::not_found("not found"))?
    } else {
        state
            .store
            .read(&relative)
            .await
            .map_err(|_| ApiError::not_found("not found"))?
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", crate::util::content_type_for(&relative))
        .header("Content-Length", body.len().to_string())
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(body))
        .map_err(|error| ApiError::internal(error))
}

async fn asset(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let path = uri.path().trim_start_matches('/').to_string();
    tracing::debug!("asset request: {path}");
    if state.is_withdrawn_asset(&path) {
        return Err(ApiError::not_found("not found"));
    }
    let range = headers.get("Range").and_then(|value| value.to_str().ok());
    let object = match state.store.serve(&path, range).await {
        Ok(object) => object,
        Err(ServeError::NotFound) => return Err(ApiError::not_found("not found")),
        Err(ServeError::RangeNotSatisfiable(size)) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{size}"))
                .header("Content-Length", "0")
                .body(Body::empty())
                .map_err(ApiError::internal)?);
        }
        Err(ServeError::Other(error)) => return Err(ApiError::internal(error)),
    };
    let body = match object.body {
        ServeBody::Local(file) => {
            let stream = ReaderStream::new(file.take(object.content_length));
            Body::from_stream(stream)
        }
        ServeBody::S3(stream) => {
            let reader = stream.into_async_read();
            Body::from_stream(ReaderStream::new(reader))
        }
        ServeBody::Remote(response) => Body::from_stream(response.bytes_stream()),
    };
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(object.status).unwrap_or(StatusCode::OK))
        .header("Content-Type", object.content_type)
        .header("Content-Length", object.content_length.to_string())
        .header("Accept-Ranges", "bytes");
    if let Some(content_range) = object.content_range {
        builder = builder.header("Content-Range", content_range);
    }
    builder
        .body(body)
        .map_err(|error| ApiError::internal(error))
}

fn embedded_static(relative: &str) -> Option<(&'static [u8], &'static str)> {
    match relative {
        "index.html" => Some((EMBEDDED_INDEX_HTML.as_bytes(), "text/html; charset=utf-8")),
        "login.html" => Some((EMBEDDED_LOGIN_HTML.as_bytes(), "text/html; charset=utf-8")),
        "setup.html" => Some((EMBEDDED_SETUP_HTML.as_bytes(), "text/html; charset=utf-8")),
        "account.html" => Some((EMBEDDED_ACCOUNT_HTML.as_bytes(), "text/html; charset=utf-8")),
        "account-auth.html" => Some((
            EMBEDDED_ACCOUNT_AUTH_HTML.as_bytes(),
            "text/html; charset=utf-8",
        )),
        "logo.svg" => Some((EMBEDDED_LOGO_SVG, "image/svg+xml")),
        "i18n.js" => Some((
            EMBEDDED_I18N_JS.as_bytes(),
            "application/javascript; charset=utf-8",
        )),
        "locales/en.json" => Some((
            EMBEDDED_LOCALE_EN.as_bytes(),
            "application/json; charset=utf-8",
        )),
        _ => None,
    }
}

async fn static_file(State(state): State<AppState>, uri: Uri) -> ApiResult<Response> {
    let path = uri.path();
    tracing::debug!("static request: {path}");
    if path == "/admin/login" || path == "/admin/login.html" {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", "/account/login")
            .header("Cache-Control", "no-store")
            .body(Body::empty())
            .map_err(ApiError::internal);
    }
    let relative = if path == "/"
        || path == "/index.html"
        || path == "/admin"
        || path == "/admin/"
        || path == "/admin/index.html"
    {
        "index.html".to_string()
    } else if path == "/login" || path == "/login.html" {
        "account-auth.html".to_string()
    } else if path == "/setup"
        || path == "/setup.html"
        || path == "/admin/setup"
        || path == "/admin/setup.html"
    {
        "setup.html".to_string()
    } else if path == "/account" || path == "/account/" || path == "/account/index.html" {
        "account.html".to_string()
    } else if path == "/account/login"
        || path == "/account/register"
        || path == "/account/reset"
        || path == "/account/forgot"
    {
        "account-auth.html".to_string()
    } else if path == "/account/logo.svg" || path == "/admin/logo.svg" {
        "logo.svg".to_string()
    } else {
        path.trim_start_matches('/').to_string()
    };
    if !crate::util::relative_path_is_safe(&relative) {
        return Err(ApiError::not_found("not found"));
    }

    // The admin UI is embedded in the binary. THEME_STATIC_DIR is an optional
    // development override; production deployments do not need the static folder.
    let disk_override = std::env::var_os("THEME_STATIC_DIR").is_some();
    let (body, content_type) = if disk_override {
        let target = state.paths.static_dir.join(&relative);
        if target.starts_with(&state.paths.static_dir) && target.is_file() {
            (
                tokio::fs::read(&target)
                    .await
                    .map_err(|_| ApiError::not_found("not found"))?,
                crate::util::content_type_for(&relative),
            )
        } else {
            match embedded_static(&relative) {
                Some((bytes, content_type)) => (bytes.to_vec(), content_type.to_string()),
                None => return Err(ApiError::not_found("not found")),
            }
        }
    } else {
        match embedded_static(&relative) {
            Some((bytes, content_type)) => (bytes.to_vec(), content_type.to_string()),
            None => return Err(ApiError::not_found("not found")),
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Content-Length", body.len().to_string())
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("X-Frame-Options", "DENY")
        .header("Referrer-Policy", "no-referrer")
        .body(Body::from(body))
        .map_err(ApiError::internal)
}

fn rate_allowed(state: &AppState, remote: &str) -> bool {
    let mut rate = state.rate.lock().unwrap();
    let now = Instant::now();
    let hits = rate.entry(remote.to_string()).or_default();
    hits.retain(|stamp| now.duration_since(*stamp) < Duration::from_secs(3600));
    if hits.len() >= MAX_SUBMISSIONS_PER_HOUR {
        return false;
    }
    hits.push(now);
    if rate.len() > 1000 {
        let keys: Vec<String> = rate.keys().take(200).cloned().collect();
        for key in keys {
            rate.remove(&key);
        }
    }
    true
}

async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> ApiResult<Response> {
    let expected = read_token(&state.paths);
    let provided = headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if expected.is_empty() || !token_matches(&expected, provided) {
        return Err(ApiError::unauthorized("访问令牌无效"));
    }
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let remote = request_ip(&headers);
    if !rate_allowed(&state, &remote) {
        return Err(ApiError::too_many("提交过于频繁，请稍后再试"));
    }
    let form = uploads::parse_multipart(multipart)
        .await
        .map_err(ApiError::bad)?;
    let author_id = form
        .author_id
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if !crate::util::valid_author_id(&author_id) {
        return Err(ApiError::bad(
            "请填写有效的作者 ID（16-128 位字母、数字或 . _ ~ -）",
        ));
    }
    let prepared = uploads::prepare_upload(form).await.map_err(ApiError::bad)?;
    let theme_id = prepared.id.clone();
    let author_hash = crate::util::author_key(&author_id);
    let author_fp = crate::util::author_fingerprint(&author_hash);
    if crate::authors::author_by_hash(&state.db, &author_hash)
        .await
        .map_err(ApiError::internal)?
        .is_none()
    {
        let _ = crate::authors::create_author(
            &state.db,
            &author_hash,
            &author_fp,
            &prepared.author,
            None,
            "pending",
        )
        .await;
    }

    let old_meta = catalog::find_theme_meta(&state.paths, &theme_id);
    if let Some((_, old)) = &old_meta {
        if let Some(owner) = &old.author_hash {
            if !owner.is_empty() && owner != &author_hash {
                return Err(ApiError::conflict("该主题 ID 属于其他作者"));
            }
        }
        if old.version >= prepared.version as i64 {
            return Err(ApiError::conflict(
                "该主题已有同版本或更高版本，请提升版本号",
            ));
        }
    }
    if catalog::pending_submissions(&state.paths)
        .iter()
        .any(|item| item.get("id").and_then(|value| value.as_str()) == Some(theme_id.as_str()))
    {
        return Err(ApiError::conflict("该主题已在审核队列中"));
    }

    let licensed = crate::authors::author_by_hash(&state.db, &author_hash)
        .await
        .map_err(ApiError::internal)?
        .map(|author| author.status == "active" && author.auto_publish)
        .unwrap_or(false);
    let owner_matches = old_meta
        .as_ref()
        .map(|(_, old)| old.author_hash.as_deref() == Some(author_hash.as_str()))
        .unwrap_or(true);
    let limit_bytes = state.limit_bytes.load(Ordering::Relaxed);
    if licensed && owner_matches {
        let themes = uploads::publish_theme(
            &state.store,
            &state.settings,
            &state.paths,
            limit_bytes,
            prepared,
            Some(author_hash.clone()),
            0,
            old_meta,
        )
        .await?;
        state.refresh_withdrawn_assets();
        let _ = crate::authors::touch_author(&state.db, &author_hash).await;
        return Ok(Json(json!({
            "ok": true,
            "pending": false,
            "autoApproved": true,
            "id": theme_id,
            "themes": themes.len(),
            "message": "作者许可有效，已自动发布",
        }))
        .into_response());
    }

    let (pending_count, _) = catalog::pending_state(&state.paths);
    if pending_count >= MAX_PENDING_SUBMISSIONS {
        return Err(ApiError::too_many("待审核队列已满，请稍后再试"));
    }
    let new_size = prepared.new_size();
    if let Err(error) = state
        .store
        .choose_location(new_size, &state.paths, limit_bytes)
        .await
    {
        return Err(ApiError::bad(error.to_string()));
    }
    let submission = crate::util::submission_id();
    uploads::write_pending(
        &state.paths,
        &prepared,
        &author_hash,
        &author_fp,
        &remote,
        &submission,
        None,
    )
    .map_err(ApiError::internal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "pending": true,
            "autoApproved": false,
            "id": theme_id,
            "submissionId": submission,
            "message": "已提交，等待管理员审核",
        })),
    )
        .into_response())
}

async fn upload_theme(
    State(state): State<AppState>,
    Extension(actor): Extension<crate::accounts::SessionInfo>,
    multipart: Multipart,
) -> ApiResult<Response> {
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let mut form = uploads::parse_multipart(multipart)
        .await
        .map_err(ApiError::bad)?;
    let upload_id = form.upload_id.clone();
    if form.package.data.is_empty() {
        if let Some(id) = upload_id.as_deref() {
            form.package = crate::resumable::assemble(&state, &actor.user_id, id).await?;
        }
    }
    let requested_id = form.id.trim().to_string();
    let owner_hash_value = form
        .owner_hash
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();
    let owner = if owner_hash_value.is_empty() {
        None
    } else {
        if !valid_author_hash(&owner_hash_value) {
            return Err(ApiError::bad("作者 hash 无效"));
        }
        Some(
            crate::authors::author_by_hash(&state.db, &owner_hash_value)
                .await
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found("作者不存在"))?,
        )
    };
    let theme_id = if requested_id.is_empty() {
        let Some(owner) = owner.as_ref() else {
            return Err(ApiError::bad("新建主题需要选择作者"));
        };
        crate::authors::allocate_unique_theme_id(&state.db, &state.paths, &owner.hash)
            .await
            .map_err(ApiError::internal)?
    } else {
        requested_id
    };
    let old_meta = catalog::find_theme_meta(&state.paths, &theme_id);
    if let (Some(owner), Some((_, old))) = (owner.as_ref(), old_meta.as_ref()) {
        if let Some(existing) = old.author_hash.as_deref() {
            if !existing.is_empty() && existing != owner.hash.as_str() {
                return Err(ApiError::conflict("该主题属于其他作者"));
            }
        }
    }
    let publish_owner = owner
        .as_ref()
        .map(|author| author.hash.clone())
        .or_else(|| {
            old_meta
                .as_ref()
                .and_then(|(_, old)| old.author_hash.clone())
                .filter(|value| !value.is_empty())
        });
    if let Some(owner) = owner.as_ref() {
        if !owner.display_name.as_deref().unwrap_or("").is_empty() {
            form.author = owner.display_name.clone().unwrap_or_default();
        }
    }
    form.id = theme_id.clone();
    let prepared = uploads::prepare_upload(form).await.map_err(ApiError::bad)?;
    let themes = uploads::publish_theme(
        &state.store,
        &state.settings,
        &state.paths,
        state.limit_bytes.load(Ordering::Relaxed),
        prepared,
        publish_owner,
        0,
        old_meta,
    )
    .await?;
    state.refresh_withdrawn_assets();
    if let Some(id) = upload_id.as_deref() {
        crate::resumable::remove_session(&state, &actor.user_id, id).await;
    }
    Ok(Json(json!({"ok": true, "id": theme_id, "themes": themes.len()})).into_response())
}

#[derive(Deserialize)]
struct SubmissionBody {
    #[serde(rename = "submissionId")]
    submission_id: String,
}

async fn approve(
    State(state): State<AppState>,
    Extension(actor): Extension<crate::accounts::SessionInfo>,
    body: Bytes,
) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let submission = value
        .get("submissionId")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let Some(directory) = catalog::pending_dir(&state.paths, submission) else {
        return Err(ApiError::not_found("待审核主题不存在"));
    };
    if !directory.is_dir() {
        return Err(ApiError::not_found("待审核主题不存在"));
    }
    let Some(meta) = catalog::pending_meta(&state.paths, &directory) else {
        return Err(ApiError::not_found("待审核信息损坏"));
    };
    let prepared =
        uploads::load_pending_upload(&state.paths, &directory, &meta).map_err(ApiError::bad)?;
    let theme_id = prepared.id.clone();
    let old_meta = catalog::find_theme_meta(&state.paths, &theme_id);
    if let Some((_, old)) = &old_meta {
        let pending_owner = meta.author_hash.clone();
        let old_owner = old.author_hash.clone().unwrap_or_default();
        if !old_owner.is_empty() && !pending_owner.is_empty() && old_owner != pending_owner {
            return Err(ApiError::conflict("该主题 ID 属于其他作者"));
        }
        if old.version > prepared.version as i64 {
            return Err(ApiError::conflict("商店中已有更高版本，不能降级发布"));
        }
    }
    let pending_size = uploads::pending_directory_size(&directory);
    let author_hash = if meta.author_hash.is_empty() {
        None
    } else {
        Some(meta.author_hash.clone())
    };
    let _ = uploads::publish_theme(
        &state.store,
        &state.settings,
        &state.paths,
        state.limit_bytes.load(Ordering::Relaxed),
        prepared,
        author_hash.clone(),
        pending_size,
        old_meta,
    )
    .await?;
    state.refresh_withdrawn_assets();
    if let Some(hash) = &author_hash {
        if valid_author_hash(hash) {
            if crate::authors::author_by_hash(&state.db, hash)
                .await
                .map_err(ApiError::internal)?
                .is_none()
            {
                let _ = crate::authors::create_author(
                    &state.db,
                    hash,
                    &meta.author_fingerprint,
                    &meta.author,
                    if meta.submitter_user_id.is_empty() {
                        None
                    } else {
                        Some(&meta.submitter_user_id)
                    },
                    "active",
                )
                .await;
            }
            let _ =
                crate::authors::set_author_status(&state.db, hash, "active", Some(&actor.user_id))
                    .await;
            let _ = crate::authors::set_auto_publish(&state.db, hash, true).await;
            if !meta.submitter_user_id.is_empty() {
                let _ = crate::authors::bind_author_to_user(
                    &state.db,
                    hash,
                    &meta.submitter_user_id,
                    Some(&meta.author),
                )
                .await;
            }
            let _ = crate::authors::touch_author(&state.db, hash).await;
        }
    }
    crate::accounts::finish_submission(
        &state.db,
        submission,
        "approved",
        Some(&actor.user_id),
        None,
    )
    .await
    .ok();
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "theme.approve",
        Some("theme"),
        Some(&theme_id),
        Some(submission),
    )
    .await
    .ok();
    if !meta.submitter_user_id.is_empty() {
        if let Ok(Some(user)) =
            crate::accounts::user_by_id(&state.db, &meta.submitter_user_id).await
        {
            if let Some(email) = user.email {
                let _ = crate::mailer::send_notice(
                    &state.paths,
                    &email,
                    "主题投稿已通过审核",
                    &format!("你的主题「{}」已通过审核并发布。", meta.name),
                )
                .await;
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&directory).await;
    let themes = catalog::catalog_themes_count(&state.paths);
    Ok(Json(json!({"ok": true, "id": theme_id, "themes": themes})).into_response())
}

async fn reject(
    State(state): State<AppState>,
    Extension(actor): Extension<crate::accounts::SessionInfo>,
    body: Bytes,
) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let submission = value
        .get("submissionId")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let Some(directory) = catalog::pending_dir(&state.paths, &submission) else {
        return Err(ApiError::not_found("待审核主题不存在"));
    };
    if !directory.is_dir() {
        return Err(ApiError::not_found("待审核主题不存在"));
    }
    let meta = catalog::pending_meta(&state.paths, &directory);
    tokio::fs::remove_dir_all(&directory)
        .await
        .map_err(|_| ApiError::internal("删除待审核目录失败"))?;
    crate::accounts::finish_submission(
        &state.db,
        &submission,
        "rejected",
        Some(&actor.user_id),
        Some(&reason),
    )
    .await
    .ok();
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "theme.reject",
        Some("theme"),
        Some(&submission),
        Some(&reason),
    )
    .await
    .ok();
    if let Some(meta) = meta {
        if !meta.submitter_user_id.is_empty() {
            if let Ok(Some(user)) =
                crate::accounts::user_by_id(&state.db, &meta.submitter_user_id).await
            {
                if let Some(email) = user.email {
                    let message = if reason.is_empty() {
                        format!("你的主题「{}」未通过审核。", meta.name)
                    } else {
                        format!("你的主题「{}」未通过审核：{reason}", meta.name)
                    };
                    let _ = crate::mailer::send_notice(
                        &state.paths,
                        &email,
                        "主题投稿审核结果",
                        &message,
                    )
                    .await;
                }
            }
        }
    }
    Ok(Json(json!({"ok": true, "submissionId": submission})).into_response())
}

#[derive(Deserialize)]
struct DeleteBody {
    id: String,
}

async fn delete_theme(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let id = value
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if !valid_theme_id(id) {
        return Err(ApiError::bad("invalid theme id"));
    }
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let Some((meta_file, meta)) = catalog::find_theme_meta(&state.paths, id) else {
        return Err(ApiError::not_found("theme not found"));
    };
    catalog::remove_theme_files(&state.store, Some(&meta_file), &meta)
        .await
        .map_err(ApiError::internal)?;
    let themes = catalog::rebuild_catalog(&state.store, &state.paths)
        .await
        .map_err(ApiError::internal)?;
    state.refresh_withdrawn_assets();
    Ok(Json(json!({"ok": true, "themes": themes.len()})).into_response())
}

async fn sync_catalog(State(state): State<AppState>) -> ApiResult<Response> {
    let themes = catalog::rebuild_catalog(&state.store, &state.paths)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "message": format!("catalog rebuilt ({} themes)", themes.len()), "themes": themes.len()})).into_response())
}

async fn test_catalog(State(state): State<AppState>) -> ApiResult<Response> {
    let token = read_token(&state.paths);
    let response = state
        .http
        .get("http://127.0.0.1:8080/catalog.json")
        .header(TOKEN_HEADER, token)
        .send()
        .await;
    match response {
        Ok(response) => {
            let status = response.status().as_u16();
            let data: Value = response.json().await.unwrap_or_else(|_| json!({}));
            let themes = data
                .get("themes")
                .and_then(|value| value.as_array())
                .map(|value| value.len())
                .unwrap_or(0);
            Ok(Json(json!({"ok": true, "status": status, "themes": themes})).into_response())
        }
        Err(error) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("目录接口不可用：{error}"),
        )),
    }
}

async fn reload_caddy(State(_state): State<AppState>) -> ApiResult<Response> {
    system::reload_caddy().await.map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("system command failed: {error}"),
        )
    })?;
    Ok(Json(json!({"ok": true, "message": "caddy reloaded"})).into_response())
}

async fn set_token(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let token = value
        .get("token")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();
    if !valid_token(token) {
        return Err(ApiError::bad("token must be 16-128 safe characters"));
    }
    set_env_key(&state.paths.token_env, "THEME_STORE_TOKEN", token).map_err(ApiError::internal)?;
    system::schedule_caddy_restart();
    Ok(Json(json!({"ok": true, "message": "token saved; caddy is restarting"})).into_response())
}

async fn generate_token(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({"ok": true, "token": random_hex(32)}))
}

async fn set_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let session = state
        .sessions
        .validate(
            &auth::session_token(&headers, auth::ADMIN_SESSION_COOKIE).unwrap_or_default(),
            "admin",
        )
        .await
        .ok_or_else(|| ApiError::unauthorized("未登录"))?;
    let value = json_body(body).await?;
    let password = value
        .get("password")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let current_password = value
        .get("currentPassword")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if password.len() < 8 || password.len() > 128 {
        return Err(ApiError::bad("密码长度需要 8-128 位"));
    }
    if current_password.is_empty() {
        return Err(ApiError::bad("请输入当前密码"));
    }
    let hash = crate::accounts::user_password_hash(&state.db, &session.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::bad("当前账号没有密码，无法修改"))?;
    let current = current_password.clone();
    let verified = tokio::task::spawn_blocking(move || bcrypt::verify(current, &hash))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| ApiError::internal(error.to_string()))?;
    if !verified {
        return Err(ApiError::bad("当前密码不正确"));
    }
    let pass = password.to_string();
    let new_hash = tokio::task::spawn_blocking(move || auth::hash_admin_password(&pass))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(ApiError::internal)?;
    crate::accounts::update_password(&state.db, &session.user_id, &new_hash)
        .await
        .map_err(ApiError::internal)?;
    if session.role == "admin" {
        set_env_key(&state.paths.admin_env, "ADMIN_PASSWORD_HASH", &new_hash)
            .map_err(ApiError::internal)?;
        let _ = tokio::fs::remove_file(&state.paths.admin_password_file).await;
    }
    state.sessions.revoke_user(&session.user_id).await;
    Ok(Json(json!({
        "ok": true,
        "message": "密码已更新，请重新登录",
        "relogin": true,
    }))
    .into_response())
}

async fn author_revoke(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let hash = value
        .get("hash")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !valid_author_hash(&hash) {
        return Err(ApiError::bad("invalid author hash"));
    }
    if crate::authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
        .is_none()
    {
        return Err(ApiError::not_found("author not found"));
    }
    crate::authors::set_author_status(&state.db, &hash, "revoked", None)
        .await
        .map_err(ApiError::internal)?;
    crate::accounts::audit(
        &state.db,
        None,
        "author.revoke",
        Some("author"),
        Some(&hash),
        None,
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "author revoked"})).into_response())
}

async fn author_delete(
    State(state): State<AppState>,
    Extension(actor): Extension<crate::accounts::SessionInfo>,
    body: Bytes,
) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let hash = value
        .get("hash")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !valid_author_hash(&hash) {
        return Err(ApiError::bad("invalid author hash"));
    }
    let Some(author) = crate::authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::not_found("author not found"));
    };
    if author.status != "revoked" && author.status != "retired" {
        return Err(ApiError::bad("请先撤销作者许可，再删除许可记录"));
    }
    crate::authors::delete_author(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?;
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.delete",
        Some("author"),
        Some(&hash),
        author.display_name.as_deref(),
    )
    .await
    .ok();
    Ok(Json(json!({"ok": true, "message": "作者许可已删除"})).into_response())
}

async fn storage_get(State(state): State<AppState>) -> Json<Value> {
    let usage = catalog::storage_usage(
        &state.store,
        &state.paths,
        state.settings.backend.as_str(),
        state.limit_bytes.load(Ordering::Relaxed),
    )
    .await;
    let job = state.jobs.snapshot();
    Json(json!({
        "ok": true,
        "stateDir": state.paths.root.to_string_lossy(),
        "backend": state.settings.backend.as_str(),
        "s3": {
            "endpoint": state.settings.s3_endpoint,
            "region": state.settings.s3_region,
            "bucket": state.settings.s3_bucket,
            "prefix": state.settings.s3_prefix,
            "pathStyle": state.settings.s3_path_style,
            "configured": !state.settings.s3_bucket.is_empty(),
            "hasCredentials": !state.settings.s3_access_key.is_empty() && !state.settings.s3_secret_key.is_empty(),
        },
        "usage": usage,
        "job": job,
    }))
}

pub(crate) fn job_json(state: &AppState) -> Value {
    let mut value = serde_json::to_value(state.jobs.snapshot()).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert("ok".to_string(), json!(true));
    }
    value
}

async fn storage_job(State(state): State<AppState>) -> Json<Value> {
    Json(job_json(&state))
}

async fn storage_test(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let settings = config::merge_s3_settings(&state.settings, &value)
        .map_err(|error| ApiError::bad(error.to_string()))?;
    if settings.backend == Backend::S3 {
        storage::test_s3_connection(&settings)
            .await
            .map_err(|error| ApiError::bad(error.to_string()))?;
        Ok(Json(json!({"ok": true, "message": "对象存储连接成功"})).into_response())
    } else {
        validate_storage_root(&settings.root.to_string_lossy(), None)
            .map_err(|error| ApiError::bad(error.to_string()))?;
        Ok(Json(json!({"ok": true, "message": "本地存储目录可用"})).into_response())
    }
}

async fn storage_root(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let root_text = value
        .get("root")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let current_root = state
        .paths
        .root
        .canonicalize()
        .unwrap_or_else(|_| state.paths.root.clone());
    let new_root = validate_storage_root(root_text, Some(&current_root))
        .map_err(|error| ApiError::bad(error.to_string()))?;
    if new_root == current_root {
        return Ok(Json(json!({"ok": true, "message": "存储目录未变化", "stateDir": new_root.to_string_lossy()})).into_response());
    }
    let migrate = value
        .get("migrate")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    if new_root.exists() {
        let non_empty = std::fs::read_dir(&new_root)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if non_empty && migrate {
            return Err(ApiError::bad("目标目录必须为空才能迁移"));
        }
    }
    if !migrate && !new_root.join("catalog.json").is_file() && !new_root.join("meta").is_dir() {
        return Err(ApiError::bad("目标目录没有主题数据；请勾选复制现有数据"));
    }
    let Some(progress) = state.jobs.start("root") else {
        return Err(ApiError::conflict("已有存储任务正在进行，请稍后再试"));
    };
    let mut settings = (*state.settings).clone();
    settings.root = new_root.clone();
    settings.limit_bytes = state.limit_bytes.load(Ordering::Relaxed);
    let paths = state.paths.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        let result = async {
            tokio::fs::create_dir_all(&new_root).await?;
            if migrate {
                storage::copy_state_tree(paths.root.clone(), new_root.clone(), progress.clone())
                    .await?;
            }
            write_storage_config(&state_clone.paths.storage_env, &settings)?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                progress.finish_ok("存储目录已更新，服务即将重启");
                system::schedule_admin_restart();
                system::schedule_caddy_restart();
            }
            Err(error) => progress.fail(&error.to_string()),
        }
    });
    Ok(Json(job_json(&state)).into_response())
}

async fn storage_backend(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let mut settings = config::merge_s3_settings(&state.settings, &value)
        .map_err(|error| ApiError::bad(error.to_string()))?;
    settings.limit_bytes = state.limit_bytes.load(Ordering::Relaxed);
    if settings.backend == Backend::S3 {
        validate_s3_settings(&settings).map_err(|error| ApiError::bad(error.to_string()))?;
    } else {
        validate_storage_root(&settings.root.to_string_lossy(), None)
            .map_err(|error| ApiError::bad(error.to_string()))?;
    }
    let target =
        storage::build_store(&settings).map_err(|error| ApiError::bad(error.to_string()))?;
    let same_target = settings.backend == state.settings.backend
        && settings.s3_endpoint == state.settings.s3_endpoint
        && settings.s3_bucket == state.settings.s3_bucket
        && settings.s3_prefix == state.settings.s3_prefix;
    let migrate = value
        .get("migrate")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
        && !same_target;
    let remove_source = value
        .get("removeSource")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        && !same_target;
    if !same_target {
        let source_keys = state
            .store
            .primary()
            .list()
            .await
            .map_err(ApiError::internal)?;
        let mut missing = Vec::new();
        for key in &source_keys {
            if !target.exists(key).await {
                missing.push(key.clone());
            }
        }
        if !missing.is_empty() && !migrate {
            return Err(ApiError::bad(format!(
                "目标存储缺少 {} 个资源文件，请勾选迁移",
                missing.len()
            )));
        }
    }
    let Some(progress) = state.jobs.start("backend") else {
        return Err(ApiError::conflict("已有存储任务正在进行，请稍后再试"));
    };
    let source = state.store.primary().clone();
    let paths = state.paths.clone();
    tokio::spawn(async move {
        let result = async {
            if migrate {
                storage::copy_assets(&source, &target, remove_source, &progress).await?;
            }
            write_storage_config(&paths.storage_env, &settings)?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                progress.finish_ok("存储后端已更新，服务即将重启");
                system::schedule_admin_restart();
            }
            Err(error) => progress.fail(&error.to_string()),
        }
    });
    Ok(Json(job_json(&state)).into_response())
}

async fn storage_limit(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let limit_mb = value
        .get("limitMb")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| ApiError::bad("请输入存储配额（MB）"))?;
    let limit_bytes = limit_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| ApiError::bad("存储配额超出范围"))?;
    config::validate_storage_limit(limit_bytes)
        .map_err(|error| ApiError::bad(error.to_string()))?;

    let usage = catalog::storage_usage(
        &state.store,
        &state.paths,
        state.settings.backend.as_str(),
        limit_bytes,
    )
    .await;
    if limit_bytes < usage.used {
        return Err(ApiError::bad(format!(
            "配额不能低于当前已用 {}",
            config::format_storage_size(usage.used)
        )));
    }
    set_env_key(
        &state.paths.storage_env,
        "THEME_STORAGE_LIMIT_BYTES",
        &limit_bytes.to_string(),
    )
    .map_err(ApiError::internal)?;
    state.limit_bytes.store(limit_bytes, Ordering::Relaxed);
    Ok(Json(json!({
        "ok": true,
        "message": "存储配额已更新",
        "limit": limit_bytes,
        "usage": usage,
    }))
    .into_response())
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::Path as AxumPath;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn wildcard_route_matches() {
        let app = Router::new().route(
            "/themes/{*path}",
            get(|AxumPath(path): AxumPath<String>| async move { path }),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/themes/foo.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"foo.jpg");
    }
}
