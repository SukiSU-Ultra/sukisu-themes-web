use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, OriginalUri, Path as AxumPath, Query, Request, State};
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

use crate::catalog;
use crate::config::{
    self, read_env, set_env_key, validate_s3_settings, validate_storage_root, write_storage_config,
    Backend, Paths, StorageSettings, CSRF_HEADER, CSRF_VALUE, CURRENT_FORMAT_VERSION,
    DISK_RESERVE_BYTES, MAX_BODY_BYTES, MAX_PENDING_SUBMISSIONS, MAX_SUBMISSIONS_PER_HOUR,
    MIN_FORMAT_VERSION, STORAGE_LIMIT_BYTES, TOKEN_HEADER,
};
use crate::jobs::JobManager;
use crate::models::{JobState, StorageUsage};
use crate::storage::{self, AssetStore, ServeBody, ServeError};
use crate::system;
use crate::uploads::{self, PublishError};
use crate::util::{
    file_extension, image_extension_allowed, now_iso, quote_query, random_hex, valid_author_hash,
    valid_theme_id, valid_token,
};

const EMBEDDED_INDEX_HTML: &str = include_str!("../static/index.html");
const EMBEDDED_LOGO_SVG: &[u8] = include_bytes!("../static/logo.svg");

#[derive(Clone)]
pub struct AppState {
    pub paths: Arc<Paths>,
    pub settings: Arc<StorageSettings>,
    pub store: AssetStore,
    pub http: reqwest::Client,
    pub jobs: JobManager,
    pub rate: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl AppState {
    pub fn new(settings: StorageSettings) -> anyhow::Result<Self> {
        let paths = Arc::new(config::load_paths(&settings));
        config::ensure_storage_dirs(&paths, &settings)?;
        let store = storage::build_store(&settings)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            paths,
            settings: Arc::new(settings),
            store,
            http,
            jobs: JobManager::new(),
            rate: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn too_many(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    fn internal(error: impl std::fmt::Display) -> Self {
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
        .route("/sync", post(sync_catalog))
        .route("/test-catalog", post(test_catalog))
        .route("/reload", post(reload_caddy))
        .route("/storage/test", post(storage_test))
        .route("/storage/root", post(storage_root))
        .route("/storage/backend", post(storage_backend))
        .layer(middleware::from_fn(csrf_guard));

    Router::new()
        .nest("/api", api)
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

fn read_token(paths: &Paths) -> String {
    read_env(&paths.token_env)
        .get("THEME_STORE_TOKEN")
        .cloned()
        .unwrap_or_default()
}

fn token_matches(expected: &str, provided: &str) -> bool {
    expected.len() == provided.len()
        && expected
            .as_bytes()
            .ct_eq(provided.as_bytes())
            .into()
}

fn request_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("local")
        .to_string()
}

async fn json_body(body: Bytes) -> ApiResult<Value> {
    if body.len() > MAX_BODY_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "请求体过大",
        ));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad("invalid JSON"))?;
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
            ("存储迁移进度", "GET", "/storage/job"),
        ] {
            endpoints.push(json!({
                "name": name,
                "method": method,
                "url": format!("{admin_api}{path}"),
                "auth": "Basic Auth",
            }));
        }
    }
    let (pending_count, pending_bytes) = catalog::pending_state(&state.paths);
    let authors_count = catalog::read_authors(&state.paths).authors.len();
    let usage = catalog::storage_usage(
        &state.store,
        &state.paths,
        state.settings.backend.as_str(),
    )
    .await;
    let storage_value = serde_json::to_value(&usage).unwrap_or_else(|_| json!({}));
    let caddy = system::service_state("caddy").await;
    let sync_timer = system::service_state("sukisu-themes-sync.timer").await;
    let admin_service = system::service_state("sukisu-theme-admin.service").await;
    Json(json!({
        "tokenSet": !token.is_empty(),
        "tokenHeader": TOKEN_HEADER,
        "formatVersion": CURRENT_FORMAT_VERSION,
        "minFormatVersion": MIN_FORMAT_VERSION,
        "supportedFormats": [MIN_FORMAT_VERSION, CURRENT_FORMAT_VERSION],
        "catalog": catalog::catalog_state(&state.paths),
        "pendingCount": pending_count,
        "pendingBytes": pending_bytes,
        "authorsCount": authors_count,
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
    let submissions = catalog::pending_submissions(&state.paths);
    let count = submissions.len();
    Json(json!({"ok": true, "submissions": submissions, "count": count}))
}

async fn authors(State(state): State<AppState>) -> Json<Value> {
    let authors = catalog::authors_list(&state.paths);
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
    let range = headers
        .get("Range")
        .and_then(|value| value.to_str().ok());
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
        "index.html" => Some((
            EMBEDDED_INDEX_HTML.as_bytes(),
            "text/html; charset=utf-8",
        )),
        "logo.svg" => Some((EMBEDDED_LOGO_SVG, "image/svg+xml")),
        _ => None,
    }
}

async fn static_file(State(state): State<AppState>, uri: Uri) -> ApiResult<Response> {
    let path = uri.path();
    tracing::debug!("static request: {path}");
    let relative = if path == "/" || path == "/index.html" {
        "index.html".to_string()
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
    let prepared = uploads::prepare_upload(form)
        .await
        .map_err(ApiError::bad)?;
    let theme_id = prepared.id.clone();
    let author_hash = crate::util::author_key(&author_id);
    let author_fp = crate::util::author_fingerprint(&author_hash);

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

    let licensed = catalog::read_authors(&state.paths)
        .authors
        .contains_key(&author_hash);
    let owner_matches = old_meta
        .as_ref()
        .map(|(_, old)| old.author_hash.as_deref() == Some(author_hash.as_str()))
        .unwrap_or(true);
    if licensed && owner_matches {
        let themes = uploads::publish_theme(
            &state.store,
            &state.settings,
            &state.paths,
            prepared,
            Some(author_hash.clone()),
            0,
            old_meta,
        )
        .await?;
        let _ = catalog::touch_author(&state.paths, &author_hash);
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
    let usage = catalog::storage_usage(
        &state.store,
        &state.paths,
        state.settings.backend.as_str(),
    )
    .await;
    if usage.used.saturating_add(new_size) > STORAGE_LIMIT_BYTES {
        return Err(ApiError::bad("1.2 GB 存储空间不足，请稍后再试"));
    }
    if usage.disk_free < new_size.saturating_add(DISK_RESERVE_BYTES) {
        return Err(ApiError::bad("主机磁盘剩余空间不足，已拒绝上传"));
    }
    let submission = crate::util::submission_id();
    uploads::write_pending(
        &state.paths,
        &prepared,
        &author_hash,
        &author_fp,
        &remote,
        &submission,
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
    multipart: Multipart,
) -> ApiResult<Response> {
    if state.jobs.is_running() {
        return Err(ApiError::conflict("存储迁移进行中，请稍后再试"));
    }
    let form = uploads::parse_multipart(multipart)
        .await
        .map_err(ApiError::bad)?;
    let prepared = uploads::prepare_upload(form)
        .await
        .map_err(ApiError::bad)?;
    let theme_id = prepared.id.clone();
    let old_meta = catalog::find_theme_meta(&state.paths, &theme_id);
    let themes = uploads::publish_theme(
        &state.store,
        &state.settings,
        &state.paths,
        prepared,
        None,
        0,
        old_meta,
    )
    .await?;
    Ok(Json(json!({"ok": true, "id": theme_id, "themes": themes.len()})).into_response())
}

#[derive(Deserialize)]
struct SubmissionBody {
    #[serde(rename = "submissionId")]
    submission_id: String,
}

async fn approve(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
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
    let prepared = uploads::load_pending_upload(&state.paths, &directory, &meta)
        .map_err(ApiError::bad)?;
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
        prepared,
        author_hash.clone(),
        pending_size,
        old_meta,
    )
    .await?;
    if let Some(hash) = &author_hash {
        if valid_author_hash(hash) {
            let _ = catalog::grant_author(&state.paths, hash);
        }
    }
    let _ = tokio::fs::remove_dir_all(&directory).await;
    let themes = catalog::catalog_themes_count(&state.paths);
    Ok(Json(json!({"ok": true, "id": theme_id, "themes": themes})).into_response())
}

async fn reject(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
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
    tokio::fs::remove_dir_all(&directory)
        .await
        .map_err(|_| ApiError::internal("删除待审核目录失败"))?;
    Ok(Json(json!({"ok": true, "submissionId": submission})).into_response())
}

#[derive(Deserialize)]
struct DeleteBody {
    id: String,
}

async fn delete_theme(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let id = value.get("id").and_then(|value| value.as_str()).unwrap_or("");
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
    system::reload_caddy()
        .await
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("system command failed: {error}")))?;
    Ok(Json(json!({"ok": true, "message": "caddy reloaded"})).into_response())
}

async fn set_token(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let token = value.get("token").and_then(|value| value.as_str()).unwrap_or("").trim();
    if !valid_token(token) {
        return Err(ApiError::bad("token must be 16-128 safe characters"));
    }
    set_env_key(&state.paths.token_env, "THEME_STORE_TOKEN", token)
        .map_err(ApiError::internal)?;
    system::schedule_caddy_restart();
    Ok(Json(json!({"ok": true, "message": "token saved; caddy is restarting"})).into_response())
}

async fn generate_token(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({"ok": true, "token": random_hex(32)}))
}

async fn set_password(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let value = json_body(body).await?;
    let password = value.get("password").and_then(|value| value.as_str()).unwrap_or("");
    if password.len() < 8 || password.len() > 128 {
        return Err(ApiError::bad("password must be 8-128 characters"));
    }
    let hash = system::hash_password(password).await.map_err(|error| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("could not hash the password: {error}"))
    })?;
    set_env_key(&state.paths.admin_env, "ADMIN_PASSWORD_HASH", &hash)
        .map_err(ApiError::internal)?;
    let _ = tokio::fs::remove_file(&state.paths.admin_password_file).await;
    system::schedule_caddy_restart();
    Ok(Json(json!({"ok": true, "message": "password saved; re-open the page and sign in again"})).into_response())
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
    let removed = catalog::revoke_author(&state.paths, &hash).map_err(ApiError::internal)?;
    if !removed {
        return Err(ApiError::not_found("author not found"));
    }
    Ok(Json(json!({"ok": true, "message": "author revoked"})).into_response())
}

async fn storage_get(State(state): State<AppState>) -> Json<Value> {
    let usage = catalog::storage_usage(
        &state.store,
        &state.paths,
        state.settings.backend.as_str(),
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

fn job_json(state: &AppState) -> Value {
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
    let root_text = value.get("root").and_then(|value| value.as_str()).unwrap_or("");
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
    let migrate = value.get("migrate").and_then(|value| value.as_bool()).unwrap_or(true);
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
    let paths = state.paths.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        let result = async {
            tokio::fs::create_dir_all(&new_root).await?;
            if migrate {
                storage::copy_state_tree(
                    paths.root.clone(),
                    new_root.clone(),
                    progress.clone(),
                )
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
    let settings = config::merge_s3_settings(&state.settings, &value)
        .map_err(|error| ApiError::bad(error.to_string()))?;
    if settings.backend == Backend::S3 {
        validate_s3_settings(&settings).map_err(|error| ApiError::bad(error.to_string()))?;
    } else {
        validate_storage_root(&settings.root.to_string_lossy(), None)
            .map_err(|error| ApiError::bad(error.to_string()))?;
    }
    let target = storage::build_store(&settings)
        .map_err(|error| ApiError::bad(error.to_string()))?;
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
        let source_keys = state.store.list().await.map_err(ApiError::internal)?;
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
    let source = state.store.clone();
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

#[cfg(test)]
mod route_tests {
    use super::*;
    use axum::body::Body;
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
