use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use axum::extract::{DefaultBodyLimit, Extension, Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::StreamExt;
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
    #[serde(default)]
    edge_nodes: Vec<String>,
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
        .route("/uploads/{id}/complete", post(complete))
        .layer(DefaultBodyLimit::max(MAX_CHUNK_SIZE as usize + 1024 * 1024))
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
    let ext = file_extension(&filename);
    let limit = if ext == ".sukisu" {
        MAX_PACKAGE_BYTES
    } else if crate::config::IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        crate::config::MAX_IMAGE_INPUT_BYTES
    } else {
        return Err(ApiError::bad("仅支持主题包和主题图片"));
    };
    if body.size == 0 || body.size > limit as u64 {
        return Err(ApiError::bad(format!(
            "文件不能超过 {} MB",
            limit / (1024 * 1024)
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

    if let Some((dir, mut meta)) = find_existing(
        &state.paths,
        &session.user_id,
        &filename,
        body.size,
        &fingerprint,
    )
    .await?
    {
        authorize_upload_nodes(&state, &mut meta).await;
        write_meta(&dir, &meta).await?;
        return Ok(Json(status_value(&state, &dir, &meta, true).await?));
    }

    let id = random_hex(16);
    let dir = state.paths.uploads.join(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(ApiError::internal)?;
    let now = now_iso();
    let mut meta = UploadMeta {
        id,
        owner_id: session.user_id,
        filename,
        size: body.size,
        chunk_size,
        total_chunks: body.size.div_ceil(chunk_size),
        fingerprint,
        created_at: now.clone(),
        updated_at: now,
        edge_nodes: Vec::new(),
    };
    authorize_upload_nodes(&state, &mut meta).await;
    write_meta(&dir, &meta).await?;
    Ok(Json(status_value(&state, &dir, &meta, false).await?))
}

async fn status(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    Ok(Json(status_value(&state, &dir, &meta, false).await?))
}

async fn upload_chunk(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath((id, index)): AxumPath<(String, u64)>,
    data: Bytes,
) -> Result<Json<Value>, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    let _upload_permit = state
        .upload_semaphore
        .acquire()
        .await
        .map_err(|_| ApiError::internal("上传队列不可用"))?;
    let _live_upload = state.performance.live.upload_guard();
    state.performance.live.upload.record(data.len() as u64);
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
    // Only chunks that also reached an edge carry a receipt, and only then is a
    // second hash needed to prove both copies agree. Hashing every chunk here was
    // pure overhead for the common primary-only session.
    if read_receipt(&dir, index)
        .await
        .is_some_and(|receipt| receipt.sha256 != crate::util::sha256_hex(&data))
    {
        return Err(ApiError::conflict(
            "分片内容与已确认回执不一致，请重新创建上传会话",
        ));
    }
    crate::upload_ingress::write_part(&dir, index, &data)
        .await
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    // The client only needs an acknowledgement per chunk. Returning the full
    // session status here made every worker scan every receipt file on each
    // request, which stalled all four transfers behind one disk walk.
    Ok(Json(json!({"ok": true, "index": index})))
}

async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    if headers
        .get(crate::config::CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        != Some(crate::config::CSRF_VALUE)
    {
        return Err(ApiError::forbidden("missing request header"));
    }
    let (dir, meta) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    let _ = tokio::fs::remove_dir_all(dir).await;
    cleanup_edges(&state, &meta).await;
    Ok(Json(json!({"ok": true, "message": "上传会话已删除"})))
}

pub async fn assemble(state: &AppState, owner_id: &str, id: &str) -> Result<FilePart, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, owner_id, id).await?;
    let missing = hydrate_all(state, &dir, &meta).await;
    if !missing.is_empty() {
        return Err(ApiError::bad("上传分片不完整或校验失败，请继续上传"));
    }
    let size = usize::try_from(meta.size).map_err(|_| ApiError::bad("主题包大小无效"))?;
    let meta_for_task = meta.clone();
    let dir_for_task = dir.clone();
    let data = tokio::task::spawn_blocking(move || {
        use std::io::Read;

        // Read each part straight into its final offset. The previous version
        // allocated a temporary buffer per chunk and copied it into the package,
        // roughly doubling memory traffic during verification.
        let mut data = vec![0u8; size];
        for index in 0..meta_for_task.total_chunks {
            let expected = expected_len(&meta_for_task, index);
            let offset = index.saturating_mul(meta_for_task.chunk_size) as usize;
            let end = offset + expected as usize;
            if end > data.len() {
                return Err(format!("上传分片大小与上传会话不一致（第 {} 个）", index + 1));
            }
            let part = dir_for_task.join(format!("{index}.part"));
            let mut file = std::fs::File::open(&part)
                .map_err(|_| format!("上传分片不完整，缺少第 {} 个分片", index + 1))?;
            let length = file.metadata().map(|m| m.len()).unwrap_or(0);
            if length != expected {
                return Err(format!("上传分片不完整，请继续上传第 {} 个分片", index + 1));
            }
            file.read_exact(&mut data[offset..end])
                .map_err(|_| format!("上传分片不完整，请继续上传第 {} 个分片", index + 1))?;
        }
        Ok::<Vec<u8>, String>(data)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::bad)?;
    let ext = file_extension(&meta.filename);
    Ok(FilePart {
        filename: meta.filename,
        ext,
        data,
    })
}

pub async fn remove_session(state: &AppState, owner_id: &str, id: &str) {
    if let Ok((dir, meta)) = load_owned_meta(&state.paths, owner_id, id).await {
        let _ = tokio::fs::remove_dir_all(dir).await;
        cleanup_edges(state, &meta).await;
    }
}

/// Resolve all references through the existing owner check before validating/publishing.
pub async fn resolve_form(
    state: &AppState,
    owner: &str,
    form: &mut crate::models::UploadForm,
) -> Result<Vec<String>, ApiError> {
    let mut ids = Vec::new();
    if form.package.data.is_empty() {
        if let Some(id) = &form.upload_id {
            form.package = assemble(state, owner, id).await?;
            if form.package.ext != ".sukisu" {
                return Err(ApiError::bad("主题包必须是 .sukisu 文件"));
            }
            ids.push(id.clone());
        }
    }
    if let Some(id) = &form.preview_upload_id {
        form.preview = assemble_image(state, owner, id).await?;
        ids.push(id.clone());
    }
    if let Some(id) = &form.avatar_upload_id {
        form.avatar = Some(assemble_image(state, owner, id).await?);
        ids.push(id.clone());
    }
    if form.screenshots.len() + form.screenshot_upload_ids.len() > crate::config::MAX_SCREENSHOTS {
        return Err(ApiError::bad("最多上传 6 张截图"));
    }
    for id in &form.screenshot_upload_ids {
        form.screenshots
            .push(assemble_image(state, owner, id).await?);
        ids.push(id.clone());
    }
    Ok(ids)
}

async fn assemble_image(state: &AppState, owner: &str, id: &str) -> Result<FilePart, ApiError> {
    let file = assemble(state, owner, id).await?;
    if !crate::config::IMAGE_EXTENSIONS.contains(&file.ext.as_str())
        || file.data.len() > crate::config::MAX_IMAGE_INPUT_BYTES
    {
        return Err(ApiError::bad("图片文件类型或大小无效"));
    }
    Ok(file)
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

async fn status_value(
    state: &AppState,
    dir: &Path,
    meta: &UploadMeta,
    resumed: bool,
) -> Result<Value, ApiError> {
    let local = received_chunks(dir, meta).await;
    let receipts = remote_receipts(dir).await;
    let config = state.distribution.config.read().await;
    let enabled = config
        .nodes
        .iter()
        .filter(|node| node.enabled && meta.edge_nodes.contains(&node.id))
        .map(|node| node.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut received = local.clone();
    received.extend(
        receipts
            .iter()
            .filter(|(_, node)| enabled.contains(node.as_str()))
            .map(|(index, _)| *index),
    );
    let mut targets = Vec::new();
    for node in config.nodes.iter().filter(|n| enabled.contains(n.id.as_str())) {
        if config.enabled && config.uploads_enabled && node.upload_enabled {
            let grant = crate::upload_ingress::Grant {
                id: meta.id.clone(),
                size: meta.size,
                chunk_size: meta.chunk_size,
                expires: chrono::Utc::now().timestamp() + crate::upload_ingress::GRANT_SECONDS,
            };
            targets.push(json!({"id": node.id, "name": node.name,
                "baseUrl": crate::distribution::endpoint(&node.base_url, &format!("_edge/uploads/{}", meta.id)).map_err(ApiError::internal)?.to_string(),
                "ticket": crate::upload_ingress::seal(&node.secret, &grant).map_err(ApiError::internal)?}));
        }
    }
    drop(enabled);
    drop(config);
    received.sort_unstable();
    received.dedup();
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
        "receivedLocal": local,
        "uploadTargets": targets,
        "receivedBytes": received_bytes,
        "resumed": resumed,
    }))
}

async fn received_chunks(dir: &Path, meta: &UploadMeta) -> Vec<u64> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };
    let mut received = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(index) = name
            .to_string_lossy()
            .strip_suffix(".part")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        if index < meta.total_chunks
            && entry
                .metadata()
                .await
                .is_ok_and(|m| m.len() == expected_len(meta, index))
        {
            received.push(index);
        }
    }
    received.sort_unstable();
    received.dedup();
    received
}

#[derive(Serialize, Deserialize)]
struct RemoteReceipt {
    node_id: String,
    sha256: String,
}

async fn authorize_upload_nodes(state: &AppState, meta: &mut UploadMeta) {
    let config = state.distribution.config.read().await;
    if config.enabled && config.uploads_enabled {
        for node in config
            .nodes
            .iter()
            .filter(|n| n.enabled && n.upload_enabled)
        {
            if !meta.edge_nodes.contains(&node.id) {
                meta.edge_nodes.push(node.id.clone());
            }
        }
    }
}

async fn read_receipt(dir: &Path, index: u64) -> Option<RemoteReceipt> {
    serde_json::from_slice(
        &tokio::fs::read(dir.join(format!("{index}.remote")))
            .await
            .ok()?,
    )
    .ok()
}

/// Scan the receipt files once instead of probing every chunk index per node.
async fn remote_receipts(dir: &Path) -> std::collections::HashMap<u64, String> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return std::collections::HashMap::new();
    };
    let mut receipts = std::collections::HashMap::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(index) = name
            .to_string_lossy()
            .strip_suffix(".remote")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(receipt) = read_receipt(dir, index).await {
            receipts.insert(index, receipt.node_id);
        }
    }
    receipts
}

async fn check_receipt_hash(dir: &Path, index: u64, hash: &str) -> Result<(), ApiError> {
    if read_receipt(dir, index)
        .await
        .is_some_and(|receipt| receipt.sha256 != hash)
    {
        return Err(ApiError::conflict(
            "分片内容与已确认回执不一致，请重新创建上传会话",
        ));
    }
    Ok(())
}

/// Node-only acknowledgement. No user session, credential or database write is delegated.
pub async fn edge_receipt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(receipt): Json<crate::upload_ingress::Receipt>,
) -> Result<Response, ApiError> {
    let node = state.distribution.authorize_node(&headers).await?;
    let config = state.distribution.config.read().await;
    if !config.uploads_enabled
        || !node.upload_enabled
        || state.maintenance.load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err(ApiError::forbidden("分布式上传已关闭"));
    }
    drop(config);
    if !crate::upload_ingress::valid_id(&receipt.id)
        || !crate::distribution::valid_hash(&receipt.sha256)
    {
        return Err(ApiError::bad("无效分片回执"));
    }
    let dir = state.paths.uploads.join(&receipt.id);
    let meta = read_meta(&dir)
        .await
        .ok_or_else(|| ApiError::not_found("上传会话不存在"))?;
    if !meta.edge_nodes.contains(&node.id)
        || receipt.index >= meta.total_chunks
        || expected_len(&meta, receipt.index) != receipt.size
    {
        return Err(ApiError::forbidden("节点无权确认此上传分片"));
    }
    check_receipt_hash(&dir, receipt.index, &receipt.sha256).await?;
    if let Ok(bytes) = tokio::fs::read(dir.join(format!("{}.part", receipt.index))).await {
        if crate::util::sha256_hex(&bytes) != receipt.sha256 {
            return Err(ApiError::conflict("分片内容冲突"));
        }
    }
    let remote = RemoteReceipt {
        node_id: node.id,
        sha256: receipt.sha256,
    };
    crate::config::write_atomic_async(
        &dir.join(format!("{}.remote", receipt.index)),
        &serde_json::to_vec(&remote).map_err(ApiError::internal)?,
        0o600,
    )
    .await
    .map_err(ApiError::internal)?;
    crate::distribution::no_store(json!({"ok": true}))
}

async fn hydrate_part(
    state: &AppState,
    dir: &Path,
    meta: &UploadMeta,
    index: u64,
) -> anyhow::Result<()> {
    let part = dir.join(format!("{index}.part"));
    if let Ok(metadata) = tokio::fs::metadata(&part).await {
        anyhow::ensure!(
            metadata.len() == expected_len(meta, index),
            "本地分片长度错误"
        );
        // Contents were already checked when the chunk (and any matching edge
        // receipt) was written. Re-reading every part here made `complete` walk
        // the whole package and stalled the verification phase for no benefit.
        return Ok(());
    }
    let receipt = read_receipt(dir, index)
        .await
        .ok_or_else(|| anyhow::anyhow!("缺少分片回执"))?;
    let node = state
        .distribution
        .config
        .read()
        .await
        .nodes
        .iter()
        .find(|n| n.enabled && n.id == receipt.node_id && meta.edge_nodes.contains(&n.id))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("分片节点已停用"))?;
    static TRANSFERS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    let _permit = TRANSFERS
        .get_or_init(|| tokio::sync::Semaphore::new(4))
        .acquire()
        .await?;
    let response = state
        .http
        .get(crate::distribution::endpoint(
            &node.base_url,
            &format!("_edge/uploads/{}/{}", meta.id, index),
        )?)
        .header(crate::distribution::SECRET_HEADER, &node.secret)
        .timeout(Duration::from_secs(180))
        .send()
        .await?
        .error_for_status()?;
    let expected = expected_len(meta, index);
    anyhow::ensure!(
        response.content_length() == Some(expected),
        "分片长度与回执不一致"
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::with_capacity(expected as usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        anyhow::ensure!(
            bytes.len() + chunk.len() <= expected as usize,
            "分片超出大小限制"
        );
        bytes.extend_from_slice(&chunk);
    }
    anyhow::ensure!(
        bytes.len() as u64 == expected && crate::util::sha256_hex(&bytes) == receipt.sha256,
        "分片 SHA-256 校验失败"
    );
    anyhow::ensure!(read_meta(dir).await.is_some(), "上传会话已取消");
    crate::upload_ingress::write_part(dir, index, &bytes).await
}

async fn hydrate_all(state: &AppState, dir: &Path, meta: &UploadMeta) -> Vec<u64> {
    futures::stream::iter((0..meta.total_chunks).map(|index| async move {
        hydrate_part(state, dir, meta, index)
            .await
            .err()
            .map(|_| index)
    }))
    .buffer_unordered(4)
    .filter_map(|index| async move { index })
    .collect()
    .await
}

async fn complete(
    State(state): State<AppState>,
    Extension(session): Extension<SessionInfo>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let (dir, meta) = load_owned_meta(&state.paths, &session.user_id, &id).await?;
    let missing = hydrate_all(&state, &dir, &meta).await;
    Ok(Json(
        json!({"ok": true, "complete": missing.is_empty(), "missing": missing}),
    ))
}

async fn cleanup_edges(state: &AppState, meta: &UploadMeta) {
    let nodes = state.distribution.config.read().await.nodes.clone();
    futures::future::join_all(
        nodes
            .iter()
            .filter(|n| meta.edge_nodes.contains(&n.id))
            .map(|node| async move {
                if let Ok(url) = crate::distribution::endpoint(
                    &node.base_url,
                    &format!("_edge/uploads/{}", meta.id),
                ) {
                    let _ = state
                        .http
                        .delete(url)
                        .header(crate::distribution::SECRET_HEADER, &node.secret)
                        .timeout(Duration::from_secs(2))
                        .send()
                        .await;
                }
            }),
    )
    .await;
}
