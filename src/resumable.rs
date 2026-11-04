use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::extract::{Extension, Path as AxumPath, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::accounts::SessionInfo;
use crate::api::{ApiError, AppState};
use crate::config::{Paths, MAX_PACKAGE_BYTES};
use crate::models::FilePart;
use crate::util::{file_extension, now_iso, random_hex};

const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const MIN_CHUNK_SIZE: u64 = 1024 * 1024;
const MAX_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
const SESSION_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Serialize, Deserialize)]
struct UploadMeta {
    id: String,
    owner_id: String,
    filename: String,
    size: u64,
    chunk_size: u64,
    total_chunks: u64,
    fingerprint: String,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitBody {
    filename: String,
    size: u64,
    #[serde(default)]
    fingerprint: String,
    /// Optional client preference. Older managers omit it; the server keeps 4 MB then.
    #[serde(default)]
    chunk_size: Option<u64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/uploads/init", post(init))
        .route("/uploads/{id}", get(status).delete(delete))
        .route("/uploads/{id}/{index}", post(upload_chunk))
}

async fn init(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    Json(body): Json<InitBody>,
) -> Result<Json<Value>, ApiError> {
    let filename = body.filename.trim().to_string();
    if filename.is_empty()
        || filename.chars().count() > 200
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
    {
        return Err(ApiError::bad("主题包文件名无效"));
    }
    if file_extension(&filename) != ".sukisu" {
        return Err(ApiError::bad("主题包必须是 .sukisu 文件"));
    }
    if body.size == 0 || body.size > MAX_PACKAGE_BYTES as u64 {
        return Err(ApiError::bad(format!(
            "主题包不能超过 {} MB",
            MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }
    let mut fingerprint = body.fingerprint.trim().to_string();
    if fingerprint.is_empty() {
        fingerprint = format!("{filename}:{}", body.size);
    }
    if fingerprint.chars().count() > 200 {
        fingerprint = fingerprint.chars().take(200).collect();
    }
    let chunk_size = body.chunk_size.unwrap_or(CHUNK_SIZE);
    if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size) {
        return Err(ApiError::bad(format!(
            "分片大小必须在 {} MB 到 {} MB 之间",
            MIN_CHUNK_SIZE / (1024 * 1024),
            MAX_CHUNK_SIZE / (1024 * 1024)
        )));
    }

    if let Some((dir, meta)) = find_existing(
        &state.paths,
        &session.user_id,
        &filename,
        body.size,
        &fingerprint,
    )
    .await?
    {
        return Ok(Json(status_value(&dir, &meta, true).await?));
    }

    let id = random_hex(16);
    let dir = state.paths.uploads.join(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(ApiError::internal)?;
    let now = now_iso();
    let meta = UploadMeta {
        id,
        owner_id: session.user_id,
        filename,
        size: body.size,
        chunk_size,
        total_chunks: body.size.div_ceil(chunk_size),
        fingerprint,
        created_at: now.clone(),
        updated_at: now,
    };
    write_meta(&dir, &meta).await?;
    Ok(Json(status_value(&dir, &meta, false).await?))
}

async fn status(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    Ok(Json(status_value(&dir, &meta, false).await?))
}

async fn upload_chunk(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath((id, index)): AxumPath<(String, u64)>,
    data: Bytes,
) -> Result<Json<Value>, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    if index >= meta.total_chunks {
        return Err(ApiError::bad("分片序号超出范围"));
    }
    let expected = expected_len(&meta, index);
    if data.len() as u64 != expected {
        return Err(ApiError::bad(format!(
            "分片大小不正确：需要 {expected} 字节，收到 {} 字节",
            data.len()
        )));
    }
    let part = dir.join(format!("{index}.part"));
    let temp = dir.join(format!(".{index}.part.tmp"));
    tokio::fs::write(&temp, &data)
        .await
        .map_err(ApiError::internal)?;
    tokio::fs::rename(&temp, &part)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(status_value(&dir, &meta, false).await?))
}

async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let (dir, _) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    let _ = tokio::fs::remove_dir_all(dir).await;
    Ok(Json(json!({"ok": true, "message": "上传会话已删除"})))
}

pub async fn assemble(state: &AppState, owner_id: &str, id: &str) -> Result<FilePart, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, owner_id, id).await?;
    let size = usize::try_from(meta.size).map_err(|_| ApiError::bad("主题包大小无效"))?;
    let mut data = Vec::with_capacity(size);
    for index in 0..meta.total_chunks {
        let expected = expected_len(&meta, index);
        let part = dir.join(format!("{index}.part"));
        let bytes = tokio::fs::read(&part)
            .await
            .map_err(|_| ApiError::bad(format!("上传分片不完整，缺少第 {} 个分片", index + 1)))?;
        if bytes.len() as u64 != expected {
            return Err(ApiError::bad(format!(
                "上传分片不完整，请继续上传第 {} 个分片",
                index + 1
            )));
        }
        data.extend_from_slice(&bytes);
    }
    if data.len() != size {
        return Err(ApiError::bad("主题包大小与上传会话不一致"));
    }
    let ext = file_extension(&meta.filename);
    Ok(FilePart {
        filename: meta.filename,
        ext,
        data,
    })
}

pub async fn remove_session(state: &AppState, owner_id: &str, id: &str) {
    if let Ok((dir, _)) = load_owned_meta(&state.paths, owner_id, id).await {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}

pub fn cleanup_stale(paths: &Paths) {
    let Ok(entries) = std::fs::read_dir(&paths.uploads) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > SESSION_MAX_AGE);
        if stale {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

async fn find_existing(
    paths: &Paths,
    owner_id: &str,
    filename: &str,
    size: u64,
    fingerprint: &str,
) -> Result<Option<(PathBuf, UploadMeta)>, ApiError> {
    let Ok(mut entries) = tokio::fs::read_dir(&paths.uploads).await else {
        return Ok(None);
    };
    while let Some(entry) = entries.next_entry().await.map_err(ApiError::internal)? {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(meta) = read_meta(&dir).await else {
            continue;
        };
        if meta.owner_id == owner_id
            && meta.filename == filename
            && meta.size == size
            && meta.fingerprint == fingerprint
        {
            return Ok(Some((dir, meta)));
        }
    }
    Ok(None)
}

async fn load_owned_meta(
    paths: &Paths,
    owner_id: &str,
    id: &str,
) -> Result<(PathBuf, UploadMeta), ApiError> {
    if !valid_id(id) {
        return Err(ApiError::not_found("上传会话不存在"));
    }
    let dir = paths.uploads.join(id);
    let meta = read_meta(&dir)
        .await
        .ok_or_else(|| ApiError::not_found("上传会话不存在"))?;
    if meta.owner_id != owner_id {
        return Err(ApiError::forbidden("无权访问该上传会话"));
    }
    Ok((dir, meta))
}

fn valid_id(value: &str) -> bool {
    (8..=64).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn expected_len(meta: &UploadMeta, index: u64) -> u64 {
    let offset = index.saturating_mul(meta.chunk_size);
    meta.size.saturating_sub(offset).min(meta.chunk_size)
}

async fn read_meta(dir: &Path) -> Option<UploadMeta> {
    let data = tokio::fs::read(dir.join("meta.json")).await.ok()?;
    serde_json::from_slice(&data).ok()
}

async fn write_meta(dir: &Path, meta: &UploadMeta) -> Result<(), ApiError> {
    let data = serde_json::to_vec(meta).map_err(ApiError::internal)?;
    let temp = dir.join(".meta.json.tmp");
    tokio::fs::write(&temp, data)
        .await
        .map_err(ApiError::internal)?;
    tokio::fs::rename(&temp, dir.join("meta.json"))
        .await
        .map_err(ApiError::internal)
}

async fn status_value(dir: &Path, meta: &UploadMeta, resumed: bool) -> Result<Value, ApiError> {
    let received = received_chunks(dir, meta).await;
    let received_bytes: u64 = received
        .iter()
        .map(|index| expected_len(meta, *index))
        .sum();
    Ok(json!({
        "ok": true,
        "uploadId": meta.id,
        "filename": meta.filename,
        "size": meta.size,
        "chunkSize": meta.chunk_size,
        "totalChunks": meta.total_chunks,
        "received": received,
        "receivedBytes": received_bytes,
        "resumed": resumed,
    }))
}

async fn received_chunks(dir: &Path, meta: &UploadMeta) -> Vec<u64> {
    let mut received = Vec::new();
    for index in 0..meta.total_chunks {
        let expected = expected_len(meta, index);
        let Ok(metadata) = tokio::fs::metadata(dir.join(format!("{index}.part"))).await else {
            continue;
        };
        if metadata.is_file() && metadata.len() == expected {
            received.push(index);
        }
    }
    received
}
