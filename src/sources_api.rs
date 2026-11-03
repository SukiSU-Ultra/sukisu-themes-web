use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::SessionInfo;
use crate::api::{job_json, json_body, ApiError, AppState};
use crate::sources::{self, StorageSource};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/storage/sources", get(list_sources))
        .route("/storage/sources/save", post(save_source))
        .route("/storage/sources/delete", post(delete_source))
        .route("/storage/sources/test", post(test_source))
        .route("/storage/sources/browse", post(browse_source))
        .route("/storage/sources/import", post(import_source))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceInput {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    writable: Option<bool>,
    #[serde(default)]
    quota_mb: Option<u64>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    local: Option<sources::LocalSource>,
    #[serde(default)]
    alist: Option<AlistInput>,
    #[serde(default)]
    clear_token: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlistInput {
    #[serde(default)]
    url: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteBody {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowseBody {
    source_id: String,
    #[serde(default)]
    path: String,
}

async fn list_sources(State(state): State<AppState>) -> Json<Value> {
    let sources = state.sources.list().await;
    let mut items = Vec::with_capacity(sources.len());
    for source in &sources {
        let mut value = sources::public_json(source);
        if source.writable {
            if let Some(map) = value.as_object_mut() {
                map.insert(
                    "usedBytes".to_string(),
                    json!(crate::catalog::location_usage(&state.paths, &source.id).await),
                );
            }
        }
        items.push(value);
    }
    Json(json!({
        "ok": true,
        "sources": items,
        "count": sources.len(),
        "enabled": sources.iter().filter(|source| source.enabled).count(),
        "errors": sources.iter().filter(|source| source.status == "error").count(),
    }))
}

async fn save_source(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let value = json_body(body).await?;
    let input: SourceInput = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("来源配置格式错误：{error}")))?;
    let source = build_source(&state, input).await?;
    if let Some(existing) = state.sources.get(&source.id).await {
        if existing.writable {
            let used = crate::catalog::location_usage(&state.paths, &existing.id).await;
            if used > 0 {
                if source.quota_bytes > 0 && source.quota_bytes < used {
                    return Err(ApiError::conflict("来源配额不能低于该位置当前已用大小"));
                }
                let changed = source.kind != existing.kind
                    || source.local.path.trim() != existing.local.path.trim()
                    || source.alist.url.trim() != existing.alist.url.trim()
                    || source.alist.root.trim() != existing.alist.root.trim()
                    || !source.writable;
                if changed {
                    return Err(ApiError::conflict(
                        "该存储位置已有主题，不能修改类型/路径或关闭可写；请先迁移或清空",
                    ));
                }
            }
        }
    }
    let saved = state
        .sources
        .save(source)
        .await
        .map_err(|error| ApiError::bad(error.to_string()))?;
    let specs = sources::location_specs(&state.sources).await;
    state.store.reload_locations(specs).await;
    let _ = crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "storage.source.save",
        Some("source"),
        Some(&saved.id),
        Some(&saved.name),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "message": "来源已保存",
        "source": sources::public_json(&saved),
    })))
}

async fn delete_source(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let value = json_body(body).await?;
    let input: DeleteBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("请求格式错误：{error}")))?;
    let id = input.id.trim();
    if id.is_empty() {
        return Err(ApiError::bad("来源 ID 不能为空"));
    }
    let existing = state.sources.get(id).await;
    if let Some(existing) = existing.as_ref() {
        if existing.writable {
            let used = crate::catalog::location_usage(&state.paths, &existing.id).await;
            if used > 0 {
                return Err(ApiError::conflict(
                    "该存储位置仍有主题，请先迁移或删除主题后再删除来源",
                ));
            }
        }
    }
    let removed = state
        .sources
        .delete(id)
        .await
        .map_err(|error| ApiError::internal(error))?;
    if !removed {
        return Err(ApiError::not_found("来源不存在"));
    }
    let specs = sources::location_specs(&state.sources).await;
    state.store.reload_locations(specs).await;
    let _ = crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "storage.source.delete",
        Some("source"),
        Some(id),
        existing.as_ref().map(|source| source.name.as_str()),
    )
    .await;
    Ok(Json(json!({"ok": true, "message": "来源已删除"})))
}

async fn test_source(State(state): State<AppState>, body: Bytes) -> Result<Json<Value>, ApiError> {
    let value = json_body(body).await?;
    let input: SourceInput = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("来源配置格式错误：{error}")))?;
    let source = build_source(&state, input).await?;
    let source_id = source.id.clone();
    match sources::test_source(&state.http, &source).await {
        Ok(message) => {
            if !source_id.is_empty() {
                state.sources.update_status(&source_id, "ok", None).await;
            }
            Ok(Json(json!({"ok": true, "message": message})))
        }
        Err(error) => {
            let message = error.to_string();
            if !source_id.is_empty() {
                state
                    .sources
                    .update_status(&source_id, "error", Some(&message))
                    .await;
            }
            Err(ApiError::bad(message))
        }
    }
}

async fn browse_source(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let value = json_body(body).await?;
    let input: BrowseBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("请求格式错误：{error}")))?;
    let source = state
        .sources
        .get(input.source_id.trim())
        .await
        .ok_or_else(|| ApiError::not_found("来源不存在"))?;
    match sources::browse_source(&state.http, &source, &input.path).await {
        Ok(result) => {
            state.sources.update_status(&source.id, "ok", None).await;
            Ok(Json(json!({
                "ok": true,
                "sourceId": source.id,
                "path": result.path,
                "entries": result.entries,
            })))
        }
        Err(error) => {
            let message = error.to_string();
            state
                .sources
                .update_status(&source.id, "error", Some(&message))
                .await;
            Err(ApiError::bad(message))
        }
    }
}

async fn import_source(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if state.jobs.is_running() {
        return Err(ApiError::conflict("已有存储任务正在进行，请稍后再试"));
    }
    let value = json_body(body).await?;
    let request: sources::ImportRequest = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("导入参数格式错误：{error}")))?;
    let source = state
        .sources
        .get(request.source_id.trim())
        .await
        .ok_or_else(|| ApiError::not_found("来源不存在"))?;
    if !source.enabled {
        return Err(ApiError::bad("该来源已禁用"));
    }
    let Some(progress) = state.jobs.start("import") else {
        return Err(ApiError::conflict("已有存储任务正在进行，请稍后再试"));
    };
    let task_state = state.clone();
    let source_id = source.id.clone();
    let actor_id = actor.user_id.clone();
    tokio::spawn(async move {
        match sources::import_theme(&task_state, &actor_id, &source, request, &progress).await {
            Ok(message) => {
                progress.finish_ok(&message);
                task_state
                    .sources
                    .update_status(&source_id, "ok", None)
                    .await;
            }
            Err(error) => {
                progress.finish_err("导入失败", &error);
                task_state
                    .sources
                    .update_status(&source_id, "error", Some(&error))
                    .await;
            }
        }
    });
    Ok(Json(job_json(&state)))
}

async fn build_source(state: &AppState, input: SourceInput) -> Result<StorageSource, ApiError> {
    let input_id = input
        .id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let existing = match input_id.as_deref() {
        Some(id) => Some(
            state
                .sources
                .get(id)
                .await
                .ok_or_else(|| ApiError::not_found("来源不存在"))?,
        ),
        None => None,
    };
    let kind = if input.kind.trim().is_empty() {
        existing
            .as_ref()
            .map(|source| source.kind.clone())
            .unwrap_or_else(|| "local".to_string())
    } else {
        input.kind.trim().to_ascii_lowercase()
    };
    let local = input.local.unwrap_or_else(|| {
        existing
            .as_ref()
            .map(|source| source.local.clone())
            .unwrap_or_default()
    });
    let mut alist = existing
        .as_ref()
        .map(|source| source.alist.clone())
        .unwrap_or_default();
    if let Some(input_alist) = input.alist {
        if !input_alist.url.trim().is_empty() {
            alist.url = input_alist.url.trim().to_string();
        }
        if !input_alist.root.trim().is_empty() {
            alist.root = input_alist.root.trim().to_string();
        }
        if !input_alist.token.trim().is_empty() {
            alist.token = input_alist.token.trim().to_string();
        }
    }
    if input.clear_token {
        alist.token.clear();
    }
    let name = if input.name.trim().is_empty() {
        existing
            .as_ref()
            .map(|source| source.name.clone())
            .unwrap_or_default()
    } else {
        input.name.trim().to_string()
    };
    let enabled = input
        .enabled
        .or_else(|| existing.as_ref().map(|source| source.enabled))
        .unwrap_or(true);
    let writable = input
        .writable
        .or_else(|| existing.as_ref().map(|source| source.writable))
        .unwrap_or(false);
    let quota_bytes = match input.quota_mb {
        Some(mb) => mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| ApiError::bad("来源配额超出范围"))?,
        None => existing
            .as_ref()
            .map(|source| source.quota_bytes)
            .unwrap_or(0),
    };
    if quota_bytes > 0 {
        crate::config::validate_storage_limit(quota_bytes)
            .map_err(|error| ApiError::bad(error.to_string()))?;
    }
    let priority = input
        .priority
        .or_else(|| existing.as_ref().map(|source| source.priority))
        .unwrap_or(0);
    let created_at = existing
        .as_ref()
        .map(|source| source.created_at.clone())
        .unwrap_or_default();
    let updated_at = existing
        .as_ref()
        .map(|source| source.updated_at.clone())
        .unwrap_or_default();
    Ok(StorageSource {
        id: input_id.unwrap_or_default(),
        name,
        kind,
        enabled,
        writable,
        quota_bytes,
        priority,
        local,
        alist,
        status: "unknown".to_string(),
        last_error: None,
        last_checked_at: None,
        created_at,
        updated_at,
    })
}
