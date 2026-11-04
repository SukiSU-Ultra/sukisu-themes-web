use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
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
use sha2::{Digest, Sha256};
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

struct MetaIndexCache {
    revision: u64,
    built_at: Instant,
    index: Option<Arc<catalog::MetaIndex>>,
}

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
    /// Optional HTTP mirrors used for package redirects.
    pub mirrors: crate::mirrors::MirrorPool,
    pub login_failures: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    withdrawn_assets: Arc<RwLock<HashSet<String>>>,
    /// Serializes in-place catalog counter writes during download bursts.
    stats_write_lock: Arc<tokio::sync::Mutex<()>>,
    /// In-memory download counts waiting for the next batch flush.
    download_buffer: Arc<Mutex<HashMap<String, u64>>>,
    /// Snapshot of per-theme counters, refreshed after each batch flush.
    theme_stats: Arc<RwLock<Arc<HashMap<String, crate::db::ThemeStats>>>>,
    meta_index: Arc<tokio::sync::Mutex<MetaIndexCache>>,
    pub started: Instant,
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
        let theme_stats = Arc::new(crate::db::load_theme_stats(&paths.db));
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
            mirrors: crate::mirrors::MirrorPool::load().await,
            login_failures: Arc::new(Mutex::new(HashMap::new())),
            withdrawn_assets,
            stats_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            download_buffer: Arc::new(Mutex::new(HashMap::new())),
            theme_stats: Arc::new(RwLock::new(theme_stats)),
            meta_index: Arc::new(tokio::sync::Mutex::new(MetaIndexCache {
                revision: 0,
                built_at: Instant::now(),
                index: None,
            })),
            started: Instant::now(),
        })
    }

    /// Count a package download in memory. The background flusher batches
    /// these into SQLite and refreshes catalog.json a few seconds later.
    pub fn record_theme_download(&self, theme_id: &str) {
        if let Ok(mut buffer) = self.download_buffer.lock() {
            *buffer.entry(theme_id.to_string()).or_insert(0) += 1;
        }
    }

    /// Persist buffered download counts and update the public catalog.
    pub async fn flush_downloads(&self) {
        let counts = {
            let Ok(mut buffer) = self.download_buffer.lock() else {
                return;
            };
            if buffer.is_empty() {
                return;
            }
            std::mem::take(&mut *buffer)
        };
        match crate::db::record_theme_downloads(&self.db, &counts).await {
            Ok(stats) => {
                if let Ok(mut guard) = self.theme_stats.write() {
                    let mut updated = (**guard).clone();
                    for (theme_id, value) in &stats {
                        updated.insert(theme_id.clone(), *value);
                    }
                    *guard = Arc::new(updated);
                }
                let _guard = self.stats_write_lock.lock().await;
                if let Err(error) = catalog::update_catalog_stats_batch(&self.paths, &stats) {
                    tracing::warn!("catalog stats update failed: {error:#}");
                }
            }
            Err(error) => {
                // Put the counts back so a temporary database failure does not
                // silently drop download metrics.
                if let Ok(mut buffer) = self.download_buffer.lock() {
                    for (theme_id, count) in counts {
                        *buffer.entry(theme_id).or_insert(0) += count;
                    }
                }
                tracing::warn!("download stats flush failed: {error:#}");
            }
        }
    }

    pub fn theme_stats(&self) -> Arc<HashMap<String, crate::db::ThemeStats>> {
        self.theme_stats
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| Arc::new(HashMap::new()))
    }

    pub fn reload_theme_stats(&self) {
        let stats = crate::db::load_theme_stats(&self.paths.db);
        if let Ok(mut guard) = self.theme_stats.write() {
            *guard = Arc::new(stats);
        }
    }

    /// Cached `meta/*.json` index. Rebuilt when metadata changes or after 30s.
    pub async fn meta_index(&self) -> Arc<catalog::MetaIndex> {
        const META_INDEX_TTL: Duration = Duration::from_secs(30);
        let mut cache = self.meta_index.lock().await;
        let revision = catalog::meta_revision();
        if let Some(index) = &cache.index {
            if cache.revision == revision && cache.built_at.elapsed() < META_INDEX_TTL {
                return index.clone();
            }
        }
        let paths = self.paths.clone();
        let index =
            match tokio::task::spawn_blocking(move || catalog::build_meta_index(&paths)).await {
                Ok(index) => Arc::new(index),
                Err(error) => {
                    tracing::warn!("meta index build failed: {error:#}");
                    // Do not cache a failed build; retry on the next request.
                    return Arc::new(catalog::MetaIndex::default());
                }
            };
        cache.index = Some(index.clone());
        cache.revision = revision;
        cache.built_at = Instant::now();
        index
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

#[derive(Debug)]
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
        .route("/stats", get(stats))
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
        .route("/author/batch", post(author_batch))
        .route("/author/restrictions", post(author_restrictions_save))
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
        .merge(crate::comments::admin_router())
        .merge(crate::resumable::router())
        .layer(middleware::from_fn_with_state(state.clone(), auth_guard))
        .layer(middleware::from_fn(csrf_guard));

    let account_api = crate::account_api::router(state.clone());

    Router::new()
        .nest("/api", api.clone())
        .nest("/admin/api", api)
        .nest("/account/api", account_api)
        .route("/submit", post(submit))
        .route("/health", get(health))
        .route("/healthz", get(health))
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
    let is_super_admin = match &session {
        Some(session) => crate::accounts::is_super_admin(&state.db, &session.user_id)
            .await
            .unwrap_or(false),
        None => false,
    };
    Json(json!({
        "ok": true,
        "authenticated": session.is_some(),
        "username": session.as_ref().map(|session| session.username.clone()),
        "role": session.as_ref().map(|session| session.role.clone()),
        "isSuperAdmin": is_super_admin,
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

pub(crate) fn read_token(paths: &Paths) -> String {
    read_env(&paths.token_env)
        .get("THEME_STORE_TOKEN")
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn token_matches(expected: &str, provided: &str) -> bool {
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
            ("下载统计", "GET", "/stats"),
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
    let (downloads_total, recent_downloads) = crate::db::download_totals(&state.db)
        .await
        .unwrap_or((0, 0));
    let theme_stats = state.theme_stats();
    let mut ranked = catalog::admin_themes(&state.paths, theme_stats.as_ref()).await;
    ranked.sort_by(|left, right| {
        theme_json_number(right, "heat")
            .cmp(&theme_json_number(left, "heat"))
            .then_with(|| {
                theme_json_number(right, "downloads").cmp(&theme_json_number(left, "downloads"))
            })
    });
    let top_themes: Vec<Value> = ranked
        .into_iter()
        .take(5)
        .map(|item| {
            json!({
                "id": theme_json_string(&item, "id"),
                "name": theme_json_string(&item, "name"),
                "author": theme_json_string(&item, "author"),
                "previewUrl": theme_json_string(&item, "previewUrl"),
                "downloads": theme_json_number(&item, "downloads"),
                "heat": theme_json_number(&item, "heat"),
            })
        })
        .collect();
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
        "stats": {
            "downloads": downloads_total,
            "recentDownloads": recent_downloads,
            "topThemes": top_themes,
        },
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

#[derive(Debug, Deserialize, Default)]
struct ThemesQuery {
    sort: Option<String>,
    order: Option<String>,
    q: Option<String>,
    tag: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

fn theme_json_string<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or("")
}

fn theme_json_number(item: &Value, key: &str) -> u64 {
    item.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn theme_matches_query(item: &Value, query: &str, tag: Option<&str>) -> bool {
    if let Some(tag) = tag {
        let tag = tag.trim().to_lowercase();
        let has_tag = item
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .any(|value| value.trim().to_lowercase() == tag)
            })
            .unwrap_or(false);
        if !has_tag {
            return false;
        }
    }
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    let mut haystack = format!(
        "{} {} {} {}",
        theme_json_string(item, "id"),
        theme_json_string(item, "name"),
        theme_json_string(item, "author"),
        theme_json_string(item, "description"),
    );
    if let Some(tags) = item.get("tags").and_then(Value::as_array) {
        for tag in tags.iter().filter_map(Value::as_str) {
            haystack.push(' ');
            haystack.push_str(tag);
        }
    }
    let haystack = haystack.to_lowercase();
    needle
        .split_whitespace()
        .all(|part| haystack.contains(part))
}

async fn themes(State(state): State<AppState>, Query(query): Query<ThemesQuery>) -> Json<Value> {
    let theme_stats = state.theme_stats();
    let mut themes = catalog::admin_themes(&state.paths, theme_stats.as_ref()).await;
    let generated_at =
        catalog::read_catalog(&state.paths).and_then(|catalog| catalog.generated_at.clone());
    themes.retain(|item| {
        theme_matches_query(item, query.q.as_deref().unwrap_or(""), query.tag.as_deref())
    });
    let total = themes.len();
    let sort = match query
        .sort
        .as_deref()
        .unwrap_or("name")
        .to_lowercase()
        .as_str()
    {
        "hot" | "heat" => "hot",
        "downloads" | "download" => "downloads",
        "updated" | "newest" => "updated",
        "size" => "size",
        "id" => "id",
        "author" => "author",
        _ => "name",
    }
    .to_string();
    let default_desc = matches!(sort.as_str(), "hot" | "downloads" | "updated" | "size");
    let descending = query
        .order
        .as_deref()
        .map(|value| value.eq_ignore_ascii_case("desc"))
        .unwrap_or(default_desc);
    themes.sort_by(|left, right| {
        let ordering = match sort.as_str() {
            "hot" | "heat" => theme_json_number(left, "heat")
                .cmp(&theme_json_number(right, "heat"))
                .then_with(|| {
                    theme_json_number(left, "downloads").cmp(&theme_json_number(right, "downloads"))
                }),
            "downloads" | "download" => theme_json_number(left, "downloads")
                .cmp(&theme_json_number(right, "downloads"))
                .then_with(|| {
                    theme_json_number(left, "heat").cmp(&theme_json_number(right, "heat"))
                }),
            "updated" | "newest" => {
                theme_json_string(left, "updatedAt").cmp(theme_json_string(right, "updatedAt"))
            }
            "size" => theme_json_number(left, "size").cmp(&theme_json_number(right, "size")),
            "id" => theme_json_string(left, "id").cmp(theme_json_string(right, "id")),
            "author" => theme_json_string(left, "author")
                .to_lowercase()
                .cmp(&theme_json_string(right, "author").to_lowercase()),
            _ => theme_json_string(left, "name")
                .to_lowercase()
                .cmp(&theme_json_string(right, "name").to_lowercase()),
        };
        let ordering = if descending {
            ordering.reverse()
        } else {
            ordering
        };
        ordering.then_with(|| theme_json_string(left, "id").cmp(theme_json_string(right, "id")))
    });
    let offset = query.offset.unwrap_or(0).min(total);
    let end = query
        .limit
        .map(|value| offset.saturating_add(value.clamp(1, 2000)).min(total))
        .unwrap_or(total);
    let limit = end - offset;
    let themes: Vec<Value> = themes.into_iter().skip(offset).take(limit).collect();
    Json(json!({
        "ok": true,
        "themes": themes,
        "total": total,
        "offset": offset,
        "limit": limit,
        "sort": sort,
        "generatedAt": generated_at,
    }))
}

async fn stats(State(state): State<AppState>) -> Json<Value> {
    let theme_stats = state.theme_stats();
    let mut themes = catalog::admin_themes(&state.paths, theme_stats.as_ref()).await;
    themes.sort_by(|left, right| {
        theme_json_number(right, "heat")
            .cmp(&theme_json_number(left, "heat"))
            .then_with(|| {
                theme_json_number(right, "downloads").cmp(&theme_json_number(left, "downloads"))
            })
    });
    let top: Vec<Value> = themes
        .into_iter()
        .take(20)
        .map(|item| {
            json!({
                "id": theme_json_string(&item, "id"),
                "name": theme_json_string(&item, "name"),
                "author": theme_json_string(&item, "author"),
                "previewUrl": theme_json_string(&item, "previewUrl"),
                "downloads": theme_json_number(&item, "downloads"),
                "heat": theme_json_number(&item, "heat"),
            })
        })
        .collect();
    let (downloads, recent) = crate::db::download_totals(&state.db)
        .await
        .unwrap_or((0, 0));
    let days = crate::db::download_daily(&state.db, 14)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(day, count)| json!({"day": day, "count": count}))
        .collect::<Vec<_>>();
    Json(json!({
        "ok": true,
        "heatRule": format!(
            "downloads + {} x downloads in the last {} days",
            crate::db::HEAT_RECENT_WEIGHT,
            crate::db::HEAT_WINDOW_DAYS
        ),
        "totals": {
            "themes": catalog::catalog_themes_count(&state.paths),
            "downloads": downloads,
            "recentDownloads": recent,
        },
        "topThemes": top,
        "days": days,
    }))
}

/// Public health/monitoring endpoint.
///
/// Returns `503` only when the database or state directory is unusable.
/// A missing catalog is reported as `degraded` because the API can still
/// rebuild it.
async fn health(State(state): State<AppState>) -> Response {
    let db_start = Instant::now();
    let db_ok = state
        .db
        .call(|conn| Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?))
        .await
        .is_ok();
    let db_latency_ms = db_start.elapsed().as_millis();
    let (storage_ok, disk_free) = match fs4::available_space(&state.paths.root) {
        Ok(free) => (true, free),
        Err(_) => (false, 0),
    };
    let catalog = catalog::read_catalog(&state.paths);
    let catalog_themes = catalog
        .as_ref()
        .map(|catalog| catalog.themes.len())
        .unwrap_or(0);
    let catalog_generated_at = catalog.and_then(|catalog| catalog.generated_at.clone());
    let (pending_count, pending_bytes) = catalog::pending_state(&state.paths);
    let (downloads, recent_downloads) = crate::db::download_totals(&state.db)
        .await
        .unwrap_or((0, 0));
    let status = if !db_ok || !storage_ok {
        "down"
    } else if catalog_themes == 0 {
        "degraded"
    } else {
        "ok"
    };
    let code = if status == "down" {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let body = json!({
        "ok": status != "down",
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "time": crate::util::now_iso(),
        "uptimeSeconds": state.started.elapsed().as_secs(),
        "checks": {
            "database": {
                "ok": db_ok,
                "latencyMs": db_latency_ms,
            },
            "storage": {
                "ok": storage_ok,
                "backend": state.settings.backend.as_str(),
                "diskFree": disk_free,
            },
            "catalog": {
                "ok": catalog_themes > 0,
                "themes": catalog_themes,
                "generatedAt": catalog_generated_at,
            },
            "pending": {
                "count": pending_count,
                "bytes": pending_bytes,
            },
        },
        "stats": {
            "downloads": downloads,
            "recentDownloads": recent_downloads,
        },
    });
    (code, Json(body)).into_response()
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

async fn authors(
    State(state): State<AppState>,
    Extension(actor): Extension<crate::accounts::SessionInfo>,
) -> Json<Value> {
    let actor_super = crate::accounts::is_super_admin(&state.db, &actor.user_id)
        .await
        .unwrap_or(false);
    let restrictions = crate::authors::all_restrictions(&state.db)
        .await
        .unwrap_or_default();
    let mut authors = crate::authors::list_authors(&state.db)
        .await
        .unwrap_or_default();
    let mut locked_by_user: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();
    for author in &mut authors {
        let Some(hash) = author.get("hash").and_then(Value::as_str) else {
            continue;
        };
        let name = author.get("name").and_then(Value::as_str);
        let owner_role = author
            .get("ownerRole")
            .and_then(Value::as_str)
            .unwrap_or("");
        let base_allowed = actor_super || owner_role == "user";
        // A reset (or a pending request) gives the account a new license; the
        // stale ones are handled from the user detail's reset button instead.
        let mut locked = false;
        if let Some(owner_id) = author.get("userId").and_then(Value::as_str) {
            if !locked_by_user.contains_key(owner_id) {
                let hashes = crate::authors::locked_author_hashes(&state.db, owner_id)
                    .await
                    .unwrap_or_default();
                locked_by_user.insert(owner_id.to_string(), hashes);
            }
            locked = locked_by_user
                .get(owner_id)
                .map(|hashes| hashes.contains(hash))
                .unwrap_or(false);
        }
        let can_restrict = base_allowed && !locked;
        let features = restrictions
            .get(hash)
            .map(|items| json!(items))
            .unwrap_or_else(|| json!([]));
        if let Some(key) =
            crate::catalog::author_avatar_key(&state.store, &state.paths, hash, name).await
        {
            if let Some(object) = author.as_object_mut() {
                object.insert(
                    "avatarUrl".to_string(),
                    json!(format!(
                        "/admin/api/preview?path={}",
                        crate::util::quote_query(&key)
                    )),
                );
            }
        }
        if let Some(object) = author.as_object_mut() {
            object.insert("restrictions".to_string(), features);
            object.insert("canRestrict".to_string(), json!(can_restrict));
        }
    }
    let count = authors.len();
    Json(json!({
        "ok": true,
        "authors": authors,
        "count": count,
        "isSuperAdmin": actor_super,
    }))
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
    if relative.starts_with("pending/") {
        let Some(path) = crate::util::safe_join(&state.paths.root, &relative) else {
            return Err(ApiError::not_found("not found"));
        };
        let body = tokio::fs::read(&path)
            .await
            .map_err(|_| ApiError::not_found("not found"))?;
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", crate::util::content_type_for(&relative))
            .header("Content-Length", body.len().to_string())
            .header("Cache-Control", "private, no-store")
            .body(Body::from(body))
            .map_err(ApiError::internal);
    }
    let object = state
        .store
        .serve(&relative, None)
        .await
        .map_err(|_| ApiError::not_found("not found"))?;
    let cache_control = if relative.starts_with("avatars/") {
        "private, max-age=300"
    } else {
        "private, max-age=3600"
    };
    serve_object_response(object, cache_control)
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
    let mirrored = uri
        .query()
        .map(|query| query.split('&').any(|part| part == "sukisu_mirror=1"))
        .unwrap_or(false);
    // Fresh package downloads may be offloaded to a configured HTTP mirror.
    // Resumed Range requests stay local so a partial file never mixes origins.
    if range.is_none() && !mirrored {
        if let Some(theme_id) = catalog::theme_id_from_package_path(&path) {
            if let Some(location) = state.mirrors.pick(&path, &headers).await {
                state.record_theme_download(&theme_id);
                let location = axum::http::HeaderValue::from_str(&location)
                    .map_err(ApiError::internal)?;
                return Ok(Response::builder()
                    .status(StatusCode::FOUND)
                    .header("Location", location)
                    .header("Cache-Control", "no-store")
                    .body(Body::empty())
                    .map_err(ApiError::internal)?);
            }
        }
    }
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
    // Count package downloads once per transfer: resumed ranges must not
    // inflate the counter. The counter is buffered in memory and flushed in
    // batches, so this stays off the request's I/O path.
    if let Some(theme_id) = catalog::theme_id_from_package_path(&path) {
        // A redirected (or self-mirror) request was already counted when the
        // 302 was issued; don't count it again when serving locally.
        let counts = !mirrored
            && range
                .map(|value| value.trim_start().starts_with("bytes=0-"))
                .unwrap_or(true);
        if counts {
            state.record_theme_download(&theme_id);
        }
    }
    // Packages and images are versioned (the key contains -v<version>), so
    // clients and Caddy can cache them safely. Author avatars stay short-lived
    // because they can be replaced in place.
    // Keep token-protected assets out of shared caches; browsers may still
    // cache them locally. Versioned keys make the long lifetimes safe.
    let cache_control = if path.starts_with("themes/") {
        "private, max-age=31536000, immutable"
    } else if path.starts_with("previews/") || path.starts_with("screenshots/") {
        "private, max-age=604800, immutable"
    } else if path.starts_with("avatars/") {
        "private, max-age=300"
    } else {
        "private, max-age=60"
    };
    serve_object_response(object, cache_control)
}

/// Streams a stored object without buffering the whole file in memory.
pub(crate) fn serve_object_response(
    object: crate::storage::ServeObject,
    cache_control: &str,
) -> ApiResult<Response> {
    let crate::storage::ServeObject {
        body,
        content_type,
        content_length,
        content_range,
        status,
    } = object;
    let body = match body {
        ServeBody::Local(file) => {
            let stream = ReaderStream::new(file.take(content_length));
            Body::from_stream(stream)
        }
        ServeBody::S3(stream) => {
            let reader = stream.into_async_read();
            Body::from_stream(ReaderStream::new(reader))
        }
        ServeBody::Remote(response) => Body::from_stream(response.bytes_stream()),
    };
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header("Content-Type", content_type)
        .header("Content-Length", content_length.to_string())
        .header("Accept-Ranges", "bytes")
        .header("Cache-Control", cache_control);
    if let Some(content_range) = content_range {
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

fn content_etag(bytes: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Sha256::digest(bytes)))
}

fn embedded_etags() -> &'static HashMap<&'static str, String> {
    static ETAGS: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
    ETAGS.get_or_init(|| {
        const FILES: &[&str] = &[
            "index.html",
            "login.html",
            "setup.html",
            "account.html",
            "account-auth.html",
            "logo.svg",
            "i18n.js",
            "locales/en.json",
        ];
        let mut map = HashMap::new();
        for file in FILES {
            if let Some((bytes, _)) = embedded_static(file) {
                map.insert(*file, content_etag(bytes));
            }
        }
        map
    })
}

async fn static_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
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
                Bytes::from(
                    tokio::fs::read(&target)
                        .await
                        .map_err(|_| ApiError::not_found("not found"))?,
                ),
                crate::util::content_type_for(&relative),
            )
        } else {
            match embedded_static(&relative) {
                Some((bytes, content_type)) => {
                    (Bytes::from_static(bytes), content_type.to_string())
                }
                None => return Err(ApiError::not_found("not found")),
            }
        }
    } else {
        match embedded_static(&relative) {
            Some((bytes, content_type)) => (Bytes::from_static(bytes), content_type.to_string()),
            None => return Err(ApiError::not_found("not found")),
        }
    };

    let etag = if disk_override {
        content_etag(&body)
    } else {
        embedded_etags()
            .get(relative.as_str())
            .cloned()
            .unwrap_or_else(|| content_etag(&body))
    };
    let cache_control = if relative.ends_with(".html") {
        "public, max-age=0, must-revalidate"
    } else {
        "public, max-age=300, must-revalidate"
    };
    if let Some(value) = headers
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
    {
        let matched = value.split(',').any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag.trim_start_matches("W/") == etag
        });
        if matched {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header("ETag", etag)
                .header("Cache-Control", cache_control)
                .header("X-Content-Type-Options", "nosniff")
                .header("X-Frame-Options", "DENY")
                .header("Referrer-Policy", "no-referrer")
                .body(Body::empty())
                .map_err(ApiError::internal);
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Content-Length", body.len().to_string())
        .header("ETag", etag)
        .header("Cache-Control", cache_control)
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
    let _ = crate::comments::clear_theme(&state.db, id).await;
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

async fn actor_can_manage_author(
    state: &AppState,
    actor: &crate::accounts::SessionInfo,
    author: &crate::authors::Author,
) -> ApiResult<bool> {
    if crate::accounts::is_super_admin(&state.db, &actor.user_id)
        .await
        .map_err(ApiError::internal)?
    {
        return Ok(true);
    }
    let Some(owner_id) = author.user_id.as_deref() else {
        return Ok(false);
    };
    Ok(crate::accounts::user_by_id(&state.db, owner_id)
        .await
        .map_err(ApiError::internal)?
        .map(|owner| owner.role == "user")
        .unwrap_or(false))
}

async fn author_revoke(
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
    if !actor_can_manage_author(&state, &actor, &author).await? {
        return Err(ApiError::forbidden("无权修改该作者许可"));
    }
    crate::authors::set_author_status(&state.db, &hash, "revoked", Some(&actor.user_id))
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
    if !actor_can_manage_author(&state, &actor, &author).await? {
        return Err(ApiError::forbidden("无权删除该作者许可"));
    }
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

async fn author_batch(
    State(state): State<AppState>,
    Extension(actor): Extension<crate::accounts::SessionInfo>,
    body: Bytes,
) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let action = value
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(action.as_str(), "revoke" | "disable" | "delete") {
        return Err(ApiError::bad("invalid batch action"));
    }
    let mut seen = HashSet::new();
    let hashes: Vec<String> = value
        .get("hashes")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|item| item.trim().to_ascii_lowercase())
                .filter(|hash| valid_author_hash(hash))
                .filter(|hash| seen.insert(hash.clone()))
                .take(200)
                .collect()
        })
        .unwrap_or_default();
    if hashes.is_empty() {
        return Err(ApiError::bad("请选择至少一个作者"));
    }
    let actor_super = crate::accounts::is_super_admin(&state.db, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    let mut updated = 0usize;
    let mut skipped = 0usize;
    let mut missing = 0usize;
    for hash in &hashes {
        let Some(author) = crate::authors::author_by_hash(&state.db, hash)
            .await
            .map_err(ApiError::internal)?
        else {
            missing += 1;
            continue;
        };
        if !actor_super {
            let allowed = match author.user_id.as_deref() {
                Some(owner_id) => crate::accounts::user_by_id(&state.db, owner_id)
                    .await
                    .map_err(ApiError::internal)?
                    .map(|owner| owner.role == "user")
                    .unwrap_or(false),
                None => false,
            };
            if !allowed {
                skipped += 1;
                continue;
            }
        }
        match action.as_str() {
            "revoke" => {
                crate::authors::set_author_status(&state.db, hash, "revoked", Some(&actor.user_id))
                    .await
                    .map_err(ApiError::internal)?;
                let _ = crate::authors::set_auto_publish(&state.db, hash, false).await;
                updated += 1;
            }
            "disable" => {
                crate::authors::set_author_status(&state.db, hash, "retired", Some(&actor.user_id))
                    .await
                    .map_err(ApiError::internal)?;
                let _ = crate::authors::set_auto_publish(&state.db, hash, false).await;
                updated += 1;
            }
            "delete" => {
                if author.status == "revoked" || author.status == "retired" {
                    crate::authors::delete_author(&state.db, hash)
                        .await
                        .map_err(ApiError::internal)?;
                    updated += 1;
                } else {
                    skipped += 1;
                }
            }
            _ => unreachable!(),
        }
    }
    let detail =
        format!("action={action}; updated={updated}; skipped={skipped}; missing={missing}");
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.batch",
        Some("author"),
        None,
        Some(&detail),
    )
    .await
    .ok();
    Ok(Json(json!({
        "ok": true,
        "action": action,
        "updated": updated,
        "skipped": skipped,
        "missing": missing,
    }))
    .into_response())
}

async fn author_restrictions_save(
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
    let author = crate::authors::author_by_hash(&state.db, &hash)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("author not found"))?;
    let actor_super = crate::accounts::is_super_admin(&state.db, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    if !actor_super {
        let Some(owner_id) = author.user_id.as_deref() else {
            return Err(ApiError::forbidden(
                "只有超级管理员可以修改未绑定账号的作者许可",
            ));
        };
        let owner = crate::accounts::user_by_id(&state.db, owner_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("作者所属账号不存在"))?;
        if owner.role != "user" {
            return Err(ApiError::forbidden(
                "普通管理员不能修改管理员账号的作者许可",
            ));
        }
    }
    let mut features = HashSet::new();
    if let Some(items) = value.get("features").and_then(|value| value.as_array()) {
        for item in items {
            let Some(feature) = item.as_str().map(str::trim) else {
                continue;
            };
            if crate::authors::AUTHOR_FEATURES.contains(&feature) {
                features.insert(feature.to_string());
            }
        }
    }
    crate::authors::set_restrictions(&state.db, &hash, &features, &actor.user_id)
        .await
        .map_err(ApiError::internal)?;
    let detail = crate::authors::AUTHOR_FEATURES
        .iter()
        .filter(|feature| features.contains(**feature))
        .copied()
        .collect::<Vec<_>>()
        .join(",");
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "author.restrictions",
        Some("author"),
        Some(&hash),
        Some(&detail),
    )
    .await
    .ok();
    Ok(Json(json!({
        "ok": true,
        "hash": hash,
        "features": features,
        "isSuperAdmin": actor_super,
    }))
    .into_response())
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
