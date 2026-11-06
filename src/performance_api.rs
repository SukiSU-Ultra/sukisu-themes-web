use axum::extract::{Extension, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::SessionInfo;
use crate::api::{json_body, ApiError, AppState};
use crate::config::{self, PerformanceSettings};
use crate::system;

type ApiResult<T> = Result<T, ApiError>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/performance", get(performance_get).post(performance_save))
        .route("/performance/stats", get(performance_stats))
        .route("/performance/live", get(performance_live))
        .route("/performance/stats/reset", post(performance_stats_reset))
        .route("/performance/restart", post(performance_restart))
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PerformanceBody {
    #[serde(default)]
    compression_enabled: Option<bool>,
    #[serde(default)]
    compression_gzip: Option<bool>,
    #[serde(default)]
    compression_zstd: Option<bool>,
    #[serde(default)]
    compression_min_bytes: Option<usize>,
    #[serde(default)]
    compression_quality: Option<String>,
    #[serde(default)]
    download_buffer_bytes: Option<usize>,
    #[serde(default)]
    upload_concurrency: Option<usize>,
    #[serde(default)]
    flush_interval_secs: Option<u64>,
    #[serde(default)]
    poll_interval_secs: Option<u64>,
    #[serde(default)]
    worker_threads: Option<usize>,
    #[serde(default)]
    blocking_threads: Option<usize>,
    #[serde(default)]
    db_pool_size: Option<usize>,
}

impl PerformanceBody {
    fn merge(self, current: &PerformanceSettings) -> PerformanceSettings {
        PerformanceSettings {
            compression_enabled: self
                .compression_enabled
                .unwrap_or(current.compression_enabled),
            compression_gzip: self.compression_gzip.unwrap_or(current.compression_gzip),
            compression_zstd: self.compression_zstd.unwrap_or(current.compression_zstd),
            compression_min_bytes: self
                .compression_min_bytes
                .unwrap_or(current.compression_min_bytes),
            compression_quality: self
                .compression_quality
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| current.compression_quality.clone()),
            download_buffer_bytes: self
                .download_buffer_bytes
                .unwrap_or(current.download_buffer_bytes),
            upload_concurrency: self
                .upload_concurrency
                .unwrap_or(current.upload_concurrency),
            flush_interval_secs: self
                .flush_interval_secs
                .unwrap_or(current.flush_interval_secs),
            poll_interval_secs: self
                .poll_interval_secs
                .unwrap_or(current.poll_interval_secs),
            worker_threads: self.worker_threads.unwrap_or(current.worker_threads),
            blocking_threads: self.blocking_threads.unwrap_or(current.blocking_threads),
            db_pool_size: self.db_pool_size.unwrap_or(current.db_pool_size),
        }
    }
}

fn restart_required(old: &PerformanceSettings, new: &PerformanceSettings) -> bool {
    old.compression_gzip != new.compression_gzip
        || old.compression_zstd != new.compression_zstd
        || old.compression_quality != new.compression_quality
        || old.upload_concurrency != new.upload_concurrency
        || old.worker_threads != new.worker_threads
        || old.blocking_threads != new.blocking_threads
        || old.db_pool_size != new.db_pool_size
}

async fn database_stats(state: &AppState, pool_size: usize) -> ApiResult<Value> {
    let sources = [
        ("account", "main", state.paths.db.clone()),
        ("authors", "authors", state.paths.authors_db.clone()),
        ("social", "social", state.paths.social_db.clone()),
        ("stats", "stats", state.paths.stats_db.clone()),
        ("audit", "audit", state.paths.audit_db.clone()),
    ];
    let query_sources: Vec<(String, String)> = sources
        .iter()
        .map(|(name, alias, _path)| (name.to_string(), alias.to_string()))
        .collect();
    let details = state
        .db
        .call(move |conn| {
            let mut result = Vec::new();
            for (_name, alias) in &query_sources {
                let mut statement = conn.prepare(&format!(
                    "SELECT name FROM {alias}.sqlite_master
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                     ORDER BY name"
                ))?;
                let names = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut tables = Vec::new();
                for name in names {
                    let escaped = name.replace('"', "\"\"");
                    let rows: i64 = conn.query_row(
                        &format!("SELECT COUNT(*) FROM {alias}.\"{escaped}\""),
                        [],
                        |row| row.get(0),
                    )?;
                    tables.push(json!({"name": name, "rows": rows}));
                }
                let page_count: i64 = conn.query_row(
                    &format!("PRAGMA {alias}.page_count"),
                    [],
                    |row| row.get(0),
                )?;
                let page_size: i64 = conn.query_row(
                    &format!("PRAGMA {alias}.page_size"),
                    [],
                    |row| row.get(0),
                )?;
                let freelist: i64 = conn.query_row(
                    &format!("PRAGMA {alias}.freelist_count"),
                    [],
                    |row| row.get(0),
                )?;
                let journal_mode: String = conn.query_row(
                    &format!("PRAGMA {alias}.journal_mode"),
                    [],
                    |row| row.get(0),
                )?;
                let auto_vacuum: i64 = conn.query_row(
                    &format!("PRAGMA {alias}.auto_vacuum"),
                    [],
                    |row| row.get(0),
                )?;
                result.push(json!({
                    "tables": tables,
                    "pageCount": page_count,
                    "pageSize": page_size,
                    "freePages": freelist,
                    "journalMode": journal_mode,
                    "autoVacuum": auto_vacuum,
                }));
            }
            Ok(result)
        })
        .await
        .map_err(ApiError::internal)?;
    let mut databases = Vec::new();
    for (index, (name, _alias, path)) in sources.iter().enumerate() {
        let size = tokio::fs::metadata(path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let wal = tokio::fs::metadata(format!("{}-wal", path.display()))
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let shm = tokio::fs::metadata(format!("{}-shm", path.display()))
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        databases.push(json!({
            "name": name,
            "path": path.to_string_lossy(),
            "size": size,
            "walSize": wal,
            "shmSize": shm,
            "details": details.get(index).cloned().unwrap_or(Value::Null),
        }));
    }
    let total_size: u64 = databases
        .iter()
        .filter_map(|item| item.get("size").and_then(Value::as_u64))
        .sum();
    Ok(json!({
        "layout": state.paths.db_layout.as_str(),
        "size": total_size,
        "poolSize": pool_size,
        "databases": databases,
    }))
}

fn route_structure() -> Value {
    json!([
        {"group": "公共页面", "paths": ["/account/*", "/i18n.js", "/locales/*"], "access": "公开"},
        {"group": "管理后台", "paths": ["/admin/*", "/api/*"], "access": "管理员会话"},
        {"group": "账号接口", "paths": ["/account/api/*"], "access": "用户会话 / 公开认证接口"},
        {"group": "主题资源", "paths": ["/themes/*", "/previews/*", "/screenshots/*", "/avatars/*", "/catalog.json"], "access": "X-Sukisu-Theme-Token"},
        {"group": "上传与下载", "paths": ["/account/api/uploads/*", "/submit", "/account/api/themes/*/download"], "access": "登录 / 令牌"},
        {"group": "运维", "paths": ["/health", "/healthz"], "access": "公开"}
    ])
}

async fn performance_get(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let settings = config::read_performance_settings(&state.paths);
    let database = database_stats(&state, settings.db_pool_size).await?;
    Ok(Json(json!({
        "ok": true,
        "settings": settings,
        "effective": {
            "compressionEnabled": state.performance.compression_enabled.load(std::sync::atomic::Ordering::Relaxed),
            "compressionMinBytes": state.performance.compression_min_bytes.load(std::sync::atomic::Ordering::Relaxed),
            "downloadBufferBytes": state.performance.download_buffer_bytes.load(std::sync::atomic::Ordering::Relaxed),
            "flushIntervalSecs": state.performance.flush_interval_secs.load(std::sync::atomic::Ordering::Relaxed),
        },
        "runtime": {
            "version": env!("CARGO_PKG_VERSION"),
            "uptimeSeconds": state.started.elapsed().as_secs(),
            "availableParallelism": std::thread::available_parallelism().map(|value| value.get()).unwrap_or(0),
            "workerThreads": settings.worker_threads,
            "blockingThreads": settings.blocking_threads,
        },
        "database": database,
        "compression": state.performance.stats.snapshot(),
        "pipeline": ["public_headers", "compression_stats", "compression (gzip/zstd)", "compression_stats_inner", "maintenance", "token_guard", "router"],
        "routes": route_structure(),
        "restartRequiredFor": ["codecs", "quality", "uploadConcurrency", "workerThreads", "blockingThreads", "dbPoolSize"],
    })))
}

async fn performance_save(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: PerformanceBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("性能配置格式错误：{error}")))?;
    let current = config::read_performance_settings(&state.paths);
    let next = config::validate_performance_settings(body.merge(&current))
        .map_err(|error| ApiError::bad(error.to_string()))?;
    config::write_performance_settings(&state.paths, &next).map_err(ApiError::internal)?;
    state.performance.apply_hot_settings(&next);
    let restart = restart_required(&current, &next);
    let _ = crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "performance.update",
        Some("config"),
        None,
        Some(&format!(
            "compression={} gzip={} zstd={} min={} quality={} buffer={} upload={} flush={} poll={} workers={} blocking={}",
            next.compression_enabled,
            next.compression_gzip,
            next.compression_zstd,
            next.compression_min_bytes,
            next.compression_quality,
            next.download_buffer_bytes,
            next.upload_concurrency,
            next.flush_interval_secs,
            next.poll_interval_secs,
            next.worker_threads,
            next.blocking_threads
        )),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "restartRequired": restart,
        "message": if restart {
            "性能配置已保存；编码/线程相关修改需要重启服务后生效"
        } else {
            "性能配置已保存，热配置立即生效"
        },
        "settings": next,
    })))
}

async fn performance_stats(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "compression": state.performance.stats.snapshot(),
    }))
}

async fn performance_live(State(state): State<AppState>) -> Json<Value> {
    let live = &state.performance.live;
    let resources: Vec<Value> = live
        .resources()
        .into_iter()
        .take(12)
        .map(|(path, active)| json!({"path": path, "active": active}))
        .collect();
    Json(json!({
        "ok": true,
        "activeRequests": live.active_requests.load(std::sync::atomic::Ordering::Relaxed),
        "activeAssets": live.active_assets.load(std::sync::atomic::Ordering::Relaxed),
        "activeUploads": live.active_uploads.load(std::sync::atomic::Ordering::Relaxed),
        "downloadBytesPerSec": live.download.bytes_per_sec(),
        "uploadBytesPerSec": live.upload.bytes_per_sec(),
        "resources": resources,
    }))
}

async fn performance_stats_reset(State(state): State<AppState>) -> Json<Value> {
    state.performance.stats.reset();
    Json(json!({"ok": true, "message": "压缩统计已重置"}))
}

async fn performance_restart(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
) -> Json<Value> {
    let _ = crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "performance.restart",
        Some("service"),
        None,
        None,
    )
    .await;
    if system::systemctl_disabled() {
        return Json(json!({
            "ok": true,
            "restart": false,
            "message": "当前环境禁用了 systemctl，请手动重启服务",
        }));
    }
    system::schedule_admin_restart();
    Json(json!({
        "ok": true,
        "restart": true,
        "message": "服务将在 2 秒后重启",
    }))
}
