//! Standalone disk-cache worker. Run the same binary with `edge <config.json>`.
//! No account database, master theme token or publishing UI exists on an edge.
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::api::ApiError;
use crate::distribution::{self, Asset, Manifest, Node, NODE_HEADER, SECRET_HEADER};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeConfig {
    pub node_id: String,
    pub secret: String,
    pub origin_url: String,
    pub cache_dir: PathBuf,
    pub bind: std::net::SocketAddr,
    pub quota_bytes: u64,
    #[serde(default = "crate::upload_ingress::default_quota")]
    pub upload_quota_bytes: u64,
}

impl EdgeConfig {
    pub fn for_node(node: &Node, origin_url: &str) -> Self {
        Self {
            node_id: node.id.clone(),
            secret: node.secret.clone(),
            origin_url: origin_url.into(),
            cache_dir: "/var/cache/sukisu-theme-edge".into(),
            bind: "127.0.0.1:8081".parse().unwrap(),
            quota_bytes: 10 * 1024 * 1024 * 1024,
            upload_quota_bytes: crate::upload_ingress::default_quota(),
        }
    }
    fn validate(&self) -> Result<()> {
        distribution::normalize_url(&self.origin_url)?;
        anyhow::ensure!(
            distribution::valid_hash(&self.secret) && !self.node_id.is_empty(),
            "无效节点身份或密钥"
        );
        anyhow::ensure!(
            self.cache_dir.is_absolute() && self.quota_bytes >= distribution::MAX_ASSET_BYTES,
            "缓存目录必须为绝对路径，配额至少为 128 MiB"
        );
        Ok(())
    }
}

#[derive(Default)]
struct Progress {
    syncing: bool,
    last_sync_at: Option<String>,
    error: Option<String>,
}

struct Edge {
    inbox: crate::upload_ingress::Inbox,
    _cache_lock: std::fs::File,
    config: EdgeConfig,
    client: reqwest::Client,
    snapshot: RwLock<Option<(Manifest, Instant)>>,
    manifest_etag: RwLock<Option<String>>,
    progress: RwLock<Progress>,
    verified: RwLock<HashSet<String>>,
    sync_lock: Mutex<()>,
    sync_wake: Notify,
    refresh_wake: Notify,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Edge {
    async fn new(config: EdgeConfig) -> Result<Arc<Self>> {
        config.validate()?;
        tokio::fs::create_dir_all(config.cache_dir.join("objects")).await?;
        let lock_path = config.cache_dir.join(".edge.lock");
        let cache_lock = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)?;
            anyhow::ensure!(
                fs4::fs_std::FileExt::try_lock_exclusive(&file)?,
                "缓存目录已被其他边缘进程使用"
            );
            Ok::<_, anyhow::Error>(file)
        })
        .await??;
        let mut entries = tokio::fs::read_dir(&config.cache_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.strip_prefix(".download-").is_some_and(|suffix| {
                suffix.len() == 24 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            }) && entry.file_type().await?.is_file()
            {
                tokio::fs::remove_file(entry.path()).await?;
            }
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let inbox = crate::upload_ingress::Inbox::new(
            config.cache_dir.join("upload-staging"),
            config.upload_quota_bytes,
        )
        .await?;
        Ok(Arc::new(Self {
            inbox,
            _cache_lock: cache_lock,
            config,
            client,
            snapshot: RwLock::new(None),
            manifest_etag: RwLock::new(None),
            progress: RwLock::new(Progress::default()),
            verified: RwLock::new(HashSet::new()),
            sync_lock: Mutex::new(()),
            sync_wake: Notify::new(),
            refresh_wake: Notify::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }))
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.config.cache_dir.join("objects").join(hash)
    }

    async fn refresh(&self) -> Result<u64> {
        let mut request = self
            .client
            .get(distribution::endpoint(
                &self.config.origin_url,
                "distribution/manifest",
            )?)
            .header(NODE_HEADER, &self.config.node_id)
            .header(SECRET_HEADER, &self.config.secret)
            .timeout(Duration::from_secs(5));
        if let Some(etag) = self.manifest_etag.read().await.clone() {
            request = request.header("If-None-Match", etag);
        }
        let response = request.send().await?;
        // Explicit revocation/maintenance failures invalidate the lease immediately.
        // Transient timeouts can use the remaining (short) existing lease.
        if matches!(response.status().as_u16(), 401 | 403 | 503) {
            *self.snapshot.write().await = None;
            *self.manifest_etag.write().await = None;
        }
        if response.status() == StatusCode::NOT_MODIFIED {
            let mut snapshot = self.snapshot.write().await;
            let (manifest, at) = snapshot.as_mut().context("主站返回 304 但本节点尚无清单")?;
            *at = Instant::now();
            self.sync_wake.notify_one();
            return Ok(manifest.poll_interval_secs);
        }
        let etag = response
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let manifest: Manifest = distribution::bounded_json(
            response.error_for_status()?,
            distribution::MAX_MANIFEST_BYTES,
        )
        .await?;
        validate_manifest(&manifest, self.config.quota_bytes)?;
        let interval = manifest.poll_interval_secs;
        *self.snapshot.write().await = Some((manifest, Instant::now()));
        *self.manifest_etag.write().await = etag;
        self.sync_wake.notify_one();
        Ok(interval)
    }

    async fn active_manifest(&self) -> Option<Manifest> {
        self.snapshot
            .read()
            .await
            .as_ref()
            .filter(|(m, at)| !m.maintenance && at.elapsed() < Duration::from_secs(m.lease_secs))
            .map(|(m, _)| m.clone())
    }

    async fn sync_all(&self) -> Result<()> {
        let Ok(_guard) = self.sync_lock.try_lock() else {
            return Ok(());
        };
        let Some(manifest) = self.active_manifest().await else {
            return Ok(());
        };
        self.progress.write().await.syncing = true;
        // Only content-addressed files managed by this cache may be evicted.
        // Never remove unrelated user files, directories, or partial transfers.
        let keep: HashSet<_> = manifest.assets.values().map(|a| a.sha256.clone()).collect();
        let result = async {
            self.evict(&keep).await?;
            let unique: HashMap<_, _> = manifest
                .assets
                .iter()
                .map(|(p, a)| (a.sha256.clone(), (p.clone(), a.clone())))
                .collect();
            futures::stream::iter(
                unique
                    .into_values()
                    .map(|(path, asset)| async move { self.ensure_asset(&path, &asset).await }),
            )
            .buffer_unordered(2)
            .try_collect::<Vec<_>>()
            .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let mut progress = self.progress.write().await;
        progress.syncing = false;
        match &result {
            Ok(()) => {
                progress.last_sync_at = Some(crate::util::now_iso());
                progress.error = None;
            }
            Err(_) => {
                progress.error = Some("资源同步失败，节点将重试；请检查源站、磁盘和配额".into());
            }
        }
        result
    }

    async fn evict(&self, keep: &HashSet<String>) -> Result<()> {
        let mut entries = tokio::fs::read_dir(self.config.cache_dir.join("objects")).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if distribution::valid_hash(&name)
                && !keep.contains(&name)
                && entry.file_type().await?.is_file()
            {
                tokio::fs::remove_file(entry.path()).await?;
                self.verified.write().await.remove(&name);
            }
        }
        Ok(())
    }

    async fn ensure_asset(&self, path: &str, asset: &Asset) -> Result<()> {
        let destination = self.object_path(&asset.sha256);
        if self.verified.read().await.contains(&asset.sha256)
            && tokio::fs::metadata(&destination)
                .await
                .is_ok_and(|m| m.is_file() && m.len() == asset.size)
        {
            return Ok(());
        }
        // A restart revalidates disk files before trusting them.
        if hash_file(&destination, asset.size)
            .await
            .is_ok_and(|hash| hash == asset.sha256)
        {
            self.verified.write().await.insert(asset.sha256.clone());
            return Ok(());
        }
        let mut url = distribution::endpoint(&self.config.origin_url, "distribution/asset")?;
        url.query_pairs_mut()
            .append_pair("path", path)
            .append_pair("sha256", &asset.sha256);
        let response = self
            .client
            .get(url)
            .header(NODE_HEADER, &self.config.node_id)
            .header(SECRET_HEADER, &self.config.secret)
            .send()
            .await?
            .error_for_status()?;
        anyhow::ensure!(
            response.content_length() == Some(asset.size),
            "资源长度与清单不一致"
        );
        let disk = self.config.cache_dir.clone();
        let free = tokio::task::spawn_blocking(move || fs4::available_space(disk)).await??;
        anyhow::ensure!(
            free > asset.size + crate::config::DISK_RESERVE_BYTES,
            "节点剩余空间不足"
        );
        let temporary = self
            .config
            .cache_dir
            .join(format!(".download-{}", crate::util::random_hex(12)));
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .await?;
            let mut stream = response.bytes_stream();
            let mut size = 0u64;
            let mut hash = Sha256::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                size += chunk.len() as u64;
                anyhow::ensure!(size <= asset.size, "资源超出声明大小");
                hash.update(&chunk);
                file.write_all(&chunk).await?;
            }
            anyhow::ensure!(
                size == asset.size && hex::encode(hash.finalize()) == asset.sha256,
                "资源 SHA-256 校验失败"
            );
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temporary, &destination).await?;
            self.verified.write().await.insert(asset.sha256.clone());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let provided = headers
            .get(SECRET_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !bool::from(self.config.secret.as_bytes().ct_eq(provided.as_bytes())) {
            return Err(ApiError::unauthorized("节点认证失败"));
        }
        Ok(())
    }
}

fn validate_manifest(manifest: &Manifest, quota: u64) -> Result<()> {
    anyhow::ensure!(
        distribution::valid_hash(&manifest.revision)
            && manifest.assets.len() <= 100_000
            && (2..=60).contains(&manifest.poll_interval_secs)
            && (5..=180).contains(&manifest.lease_secs)
            && manifest.cache_ttl_secs <= 300,
        "无效同步清单"
    );
    let mut unique = HashMap::new();
    for (path, asset) in &manifest.assets {
        anyhow::ensure!(
            distribution::valid_asset_path(path)
                && distribution::valid_hash(&asset.sha256)
                && asset.size > 0
                && asset.size <= distribution::MAX_ASSET_BYTES,
            "无效同步资源"
        );
        if let Some(size) = unique.insert(&asset.sha256, asset.size) {
            anyhow::ensure!(size == asset.size, "资源摘要大小冲突");
        }
    }
    let size = unique
        .values()
        .try_fold(0u64, |sum, n| sum.checked_add(*n))
        .context("资源大小溢出")?;
    anyhow::ensure!(size <= quota, "已发布资源超过节点配额，请增加 quotaBytes");
    Ok(())
}

async fn hash_file(path: &FsPath, expected: u64) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    anyhow::ensure!(file.metadata().await?.len() == expected, "缓存文件长度错误");
    let mut buffer = vec![0; 128 * 1024];
    let mut hash = Sha256::new();
    let mut total = 0;
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        total += n as u64;
        anyhow::ensure!(total <= expected, "缓存文件超出限制");
        hash.update(&buffer[..n]);
    }
    anyhow::ensure!(total == expected, "缓存文件长度错误");
    Ok(hex::encode(hash.finalize()))
}

fn router(edge: Arc<Edge>) -> Router {
    Router::new()
        .route("/_edge/status", get(edge_status))
        .route("/_edge/sync", post(edge_sync))
        .route(
            "/_edge/uploads/{id}",
            axum::routing::delete(edge_upload_delete),
        )
        .route(
            "/_edge/uploads/{id}/{index}",
            post(edge_upload)
                .get(edge_upload_read)
                .options(edge_upload_options),
        )
        .route(
            "/healthz",
            get(|| async {
                (
                    [("Cache-Control", "no-store")],
                    Json(json!({"ok": true, "role": "edge"})),
                )
            }),
        )
        .route("/themes/{*path}", get(edge_asset))
        .route("/previews/{*path}", get(edge_asset))
        .route("/screenshots/{*path}", get(edge_asset))
        .route("/avatars/{*path}", get(edge_asset))
        .layer(DefaultBodyLimit::max(
            crate::upload_ingress::MAX_CHUNK as usize,
        ))
        .layer(axum::middleware::from_fn({
            let edge = edge.clone();
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let edge = edge.clone();
                async move {
                    let upload = request.uri().path().starts_with("/_edge/uploads/");
                    let origin = request.headers().get("Origin").cloned();
                    let mut response = next.run(request).await;
                    if upload {
                        response
                            .headers_mut()
                            .insert("Cache-Control", "no-store".parse().unwrap());
                        response
                            .headers_mut()
                            .insert("Referrer-Policy", "no-referrer".parse().unwrap());
                        if origin.as_ref().and_then(|v| v.to_str().ok())
                            == Some(
                                reqwest::Url::parse(&edge.config.origin_url)
                                    .unwrap()
                                    .origin()
                                    .ascii_serialization()
                                    .as_str(),
                            )
                        {
                            response
                                .headers_mut()
                                .insert("Access-Control-Allow-Origin", origin.unwrap());
                            response
                                .headers_mut()
                                .insert("Vary", "Origin".parse().unwrap());
                            response.headers_mut().insert(
                                "Access-Control-Allow-Methods",
                                "POST, OPTIONS".parse().unwrap(),
                            );
                            response.headers_mut().insert(
                                "Access-Control-Allow-Headers",
                                "Content-Type, X-Requested-With, X-Sukisu-Upload-Ticket"
                                    .parse()
                                    .unwrap(),
                            );
                        }
                    }
                    if response.status().is_client_error() || response.status().is_server_error() {
                        response
                            .headers_mut()
                            .insert("Cache-Control", "no-store".parse().unwrap());
                    }
                    response
                }
            }
        }))
        .with_state(edge)
}

async fn edge_status(
    State(edge): State<Arc<Edge>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    edge.authorize(&headers)?;
    let snapshot = edge.snapshot.read().await;
    let progress = edge.progress.read().await;
    let verified = edge.verified.read().await;
    let manifest = snapshot.as_ref().map(|(m, _)| m);
    let files_total = manifest.map(|m| m.assets.len()).unwrap_or(0);
    let files_ready = manifest
        .map(|m| {
            m.assets
                .values()
                .filter(|a| verified.contains(&a.sha256))
                .count()
        })
        .unwrap_or(0);
    let cache_bytes: u64 = manifest
        .map(|m| {
            m.assets
                .values()
                .filter(|a| verified.contains(&a.sha256))
                .map(|a| (&a.sha256, a.size))
                .collect::<HashMap<_, _>>()
                .values()
                .sum()
        })
        .unwrap_or(0);
    let lease_valid = snapshot
        .as_ref()
        .is_some_and(|(m, at)| !m.maintenance && at.elapsed() < Duration::from_secs(m.lease_secs));
    distribution::no_store(json!({"ready": lease_valid && files_ready == files_total,
        "uploadReady": lease_valid && manifest.is_some_and(|m| m.uploads_enabled),
        "uploadBytes": edge.inbox.usage().await.unwrap_or(0), "uploadQuotaBytes": edge.config.upload_quota_bytes,
        "state": if lease_valid { "online" } else { "stale" }, "syncing": progress.syncing,
        "manifestRevision": manifest.map(|m| &m.revision), "filesReady": files_ready, "filesTotal": files_total,
        "cacheBytes": cache_bytes, "quotaBytes": edge.config.quota_bytes, "lastSyncAt": progress.last_sync_at,
        "error": progress.error, "leaseAgeSeconds": snapshot.as_ref().map(|(_, at)| at.elapsed().as_secs()),
        "hits": edge.hits.load(Ordering::Relaxed), "misses": edge.misses.load(Ordering::Relaxed)}))
}

async fn edge_upload_options() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn edge_upload(
    State(edge): State<Arc<Edge>>,
    Path((id, index)): Path<(String, u64)>,
    request: Request,
) -> Result<Response, ApiError> {
    use crate::upload_ingress::{self, TICKET_HEADER};
    let token = request
        .headers()
        .get(TICKET_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let grant = upload_ingress::open(&edge.config.secret, token, chrono::Utc::now().timestamp())
        .filter(|grant| grant.id == id && grant.expected_len(index).is_some())
        .ok_or_else(|| ApiError::forbidden("上传凭证无效或已过期，请重新初始化上传"))?;
    if !edge
        .active_manifest()
        .await
        .is_some_and(|m| m.uploads_enabled)
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "节点上传已停用，请改走主站",
        ));
    }
    if let Some(origin) = request.headers().get("Origin") {
        let allowed = reqwest::Url::parse(&edge.config.origin_url)
            .map_err(ApiError::internal)?
            .origin()
            .ascii_serialization();
        if origin.to_str().ok() != Some(allowed.as_str()) {
            return Err(ApiError::forbidden("上传来源不匹配"));
        }
    }
    let _permit = edge.inbox.permits.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "节点上传繁忙，请改走其他入口",
        )
    })?;
    let limit = grant.expected_len(index).unwrap() as usize;
    let bytes = tokio::time::timeout(
        Duration::from_secs(120),
        axum::body::to_bytes(request.into_body(), limit),
    )
    .await
    .map_err(|_| ApiError::bad("分片上传超时"))?
    .map_err(|_| ApiError::bad("分片长度错误"))?;
    let receipt = edge
        .inbox
        .put(&grant, index, &bytes)
        .await
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    // Acknowledgement means the primary has recorded this exact chunk location.
    // A lost callback can be retried without overwriting staged bytes.
    let response = edge
        .client
        .post(
            distribution::endpoint(&edge.config.origin_url, "distribution/upload/receipt")
                .map_err(ApiError::internal)?,
        )
        .header(NODE_HEADER, &edge.config.node_id)
        .header(SECRET_HEADER, &edge.config.secret)
        .timeout(Duration::from_secs(10))
        .json(&receipt)
        .send()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "主站未确认分片，请重试或改走主站"))?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "主站拒绝分片回执，请重试或改走主站",
        ));
    }
    distribution::no_store(
        json!({"ok": true, "nodeId": edge.config.node_id, "index": index, "sha256": receipt.sha256}),
    )
}

async fn edge_upload_read(
    State(edge): State<Arc<Edge>>,
    Path((id, index)): Path<(String, u64)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    edge.authorize(&headers)?;
    let _permit = edge
        .inbox
        .permits
        .try_acquire()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "暂存读取繁忙"))?;
    let bytes = edge
        .inbox
        .read(&id, index)
        .await
        .map_err(|_| ApiError::not_found("上传暂存分片不存在或已过期"))?;
    Response::builder()
        .header("Cache-Control", "no-store")
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", bytes.len())
        .body(Body::from(bytes))
        .map_err(ApiError::internal)
}

async fn edge_upload_delete(
    State(edge): State<Arc<Edge>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    edge.authorize(&headers)?;
    edge.inbox.remove(&id).await.map_err(ApiError::internal)?;
    distribution::no_store(json!({"ok": true}))
}

async fn edge_sync(
    State(edge): State<Arc<Edge>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    edge.authorize(&headers)?;
    edge.refresh_wake.notify_one();
    distribution::no_store(json!({"ok": true, "queued": true}))
}

#[derive(Deserialize)]
struct DownloadQuery {
    #[serde(default)]
    ticket: String,
}

async fn edge_asset(
    State(edge): State<Arc<Edge>>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let path = percent_encoding::percent_decode_str(uri.path().trim_start_matches('/'))
        .decode_utf8()
        .map_err(|_| ApiError::not_found("无效资源路径"))?
        .into_owned();
    let (digest, expires) = distribution::verify_ticket(
        &edge.config.secret,
        &query.ticket,
        &path,
        chrono::Utc::now().timestamp(),
    )
    .ok_or_else(|| ApiError::forbidden("下载凭证无效或已过期，请从主站重新获取下载地址"))?;
    let snapshot = edge.snapshot.read().await;
    let (manifest, at) = snapshot
        .as_ref()
        .filter(|(m, at)| !m.maintenance && at.elapsed() < Duration::from_secs(m.lease_secs))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "同步租约已过期，请返回主站下载",
            )
        })?;
    let asset = manifest
        .assets
        .get(&path)
        .filter(|a| a.sha256 == digest)
        .ok_or_else(|| ApiError::not_found("资源已撤回或更新"))?;
    if !edge.verified.read().await.contains(&digest) {
        edge.misses.fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "资源尚未同步，请返回主站下载",
        ));
    }
    let mut file = tokio::fs::File::open(edge.object_path(&digest))
        .await
        .map_err(|_| ApiError::not_found("缓存文件不存在"))?;
    if file.metadata().await.map_err(ApiError::internal)?.len() != asset.size {
        return Err(ApiError::not_found("缓存文件已损坏"));
    }
    let range = headers.get("Range").and_then(|v| v.to_str().ok());
    let (start, end, status) = match crate::util::parse_byte_range(range, asset.size) {
        crate::util::ByteRange::Full => (0, asset.size - 1, 200),
        crate::util::ByteRange::Partial { start, end } => (start, end, 206),
        crate::util::ByteRange::Invalid => {
            return Response::builder()
                .status(416)
                .header("Content-Range", format!("bytes */{}", asset.size))
                .header("Cache-Control", "no-store")
                .body(Body::empty())
                .map_err(ApiError::internal)
        }
    };
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(ApiError::internal)?;
    let lease_left = manifest.lease_secs.saturating_sub(at.elapsed().as_secs());
    let ttl = manifest
        .cache_ttl_secs
        .min((expires - chrono::Utc::now().timestamp()).max(0) as u64)
        .min(lease_left);
    let size = asset.size;
    drop(snapshot);
    edge.hits.fetch_add(1, Ordering::Relaxed);
    let object = crate::storage::ServeObject {
        body: crate::storage::ServeBody::Local(file),
        content_type: crate::util::content_type_for(&path),
        content_length: end - start + 1,
        content_range: (status == 206).then(|| format!("bytes {start}-{end}/{size}")),
        status,
    };
    // Disk caching does the CDN work. Never make protected responses public to shared proxies.
    let mut response = crate::api::serve_object_response(
        object,
        &format!("private, max-age={ttl}, must-revalidate"),
        128 * 1024,
    )?;
    response.headers_mut().insert(
        "X-Sukisu-Asset-Sha256",
        digest.parse().map_err(ApiError::internal)?,
    );
    if path.starts_with("themes/") {
        let filename = path
            .rsplit('/')
            .next()
            .unwrap_or("theme.sukisu")
            .replace(['"', '\\'], "_");
        response.headers_mut().insert(
            "Content-Disposition",
            format!("attachment; filename=\"{filename}\"")
                .parse()
                .map_err(ApiError::internal)?,
        );
    }
    response
        .headers_mut()
        .insert("X-Sukisu-Cache", "HIT".parse().unwrap());
    response
        .headers_mut()
        .insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert("Referrer-Policy", "no-referrer".parse().unwrap());
    Ok(response)
}

pub async fn run(file: &FsPath) -> Result<()> {
    let raw = tokio::fs::read(file)
        .await
        .context("读取边缘节点配置失败")?;
    let edge = Edge::new(serde_json::from_slice(&raw)?).await?;
    let refresh = edge.clone();
    tokio::spawn(async move {
        loop {
            let interval = match refresh.refresh().await {
                Ok(interval) => interval,
                Err(_) => {
                    refresh.progress.write().await.error =
                        Some("无法获取主站清单，请检查网络、配置和节点密钥".into());
                    5
                }
            };
            tokio::select! { _ = tokio::time::sleep(Duration::from_secs(interval)) => {}, _ = refresh.refresh_wake.notified() => {} }
        }
    });
    let worker = edge.clone();
    tokio::spawn(async move {
        loop {
            worker.sync_wake.notified().await;
            if let Err(error) = worker.sync_all().await {
                tracing::warn!("edge asset sync failed: {error}");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(edge.config.bind).await?;
    tracing::info!(address = %edge.config.bind, "theme cache edge started");
    axum::serve(listener, router(edge)).await?;
    Ok(())
}
