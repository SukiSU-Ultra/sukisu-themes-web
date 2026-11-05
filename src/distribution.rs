//! Control plane for authenticated, pull-based home-broadband cache nodes.
//! Database writes stay on the primary; edges cache published files and stage scoped uploads.
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, RwLock};

use crate::accounts::SessionInfo;
use crate::api::{ApiError, AppState};
use crate::storage::ServeError;

pub const NODE_HEADER: &str = "X-Sukisu-Node";
pub const SECRET_HEADER: &str = "X-Sukisu-Node-Key";
pub const TICKET_SECONDS: i64 = 120;
pub const MAX_ASSET_BYTES: u64 = crate::config::MAX_PACKAGE_BYTES as u64;
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
type ApiResult<T> = Result<T, ApiError>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct DistributionConfig {
    pub enabled: bool,
    pub uploads_enabled: bool,
    pub origin_url: String,
    pub strategy: String,
    pub sync_interval_secs: u64,
    pub cache_ttl_secs: u64,
    pub nodes: Vec<Node>,
}

impl Default for DistributionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            uploads_enabled: true,
            origin_url: String::new(),
            strategy: "latency".into(),
            sync_interval_secs: 5,
            cache_ttl_secs: 60,
            nodes: Vec::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub secret: String,
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub upload_enabled: bool,
    pub priority: i32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default)]
    pub uploads_enabled: bool,
    pub revision: String,
    pub assets: BTreeMap<String, Asset>,
    pub maintenance: bool,
    pub poll_interval_secs: u64,
    pub lease_secs: u64,
    pub cache_ttl_secs: u64,
}

#[derive(Clone)]
struct Probe {
    at: Instant,
    latency_ms: u64,
    status: Value,
}

pub struct Distribution {
    file: PathBuf,
    pub config: RwLock<DistributionConfig>,
    manifest: Mutex<Option<(u64, Manifest)>>,
    probes: RwLock<HashMap<String, Probe>>,
    routes: Mutex<HashMap<String, (Instant, Option<String>)>>,
    client: reqwest::Client,
}

impl Distribution {
    pub fn load(
        paths: &crate::config::Paths,
        family: crate::net::IpFamily,
    ) -> anyhow::Result<Arc<Self>> {
        let file = std::env::var_os("THEME_DISTRIBUTION_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| paths.admin_config.with_file_name("distribution.json"));
        let config = match std::fs::read(&file) {
            Ok(raw) => serde_json::from_slice::<DistributionConfig>(&raw)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DistributionConfig::default()
            }
            Err(error) => return Err(error.into()),
        };
        validate_config(&config)?;
        let client = crate::net::apply_client_family(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none()),
            family,
        )
        .build()?;
        Ok(Arc::new(Self {
            file,
            config: RwLock::new(config),
            manifest: Mutex::new(None),
            probes: RwLock::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            client,
        }))
    }

    async fn persist(&self, config: &DistributionConfig) -> ApiResult<()> {
        crate::config::write_atomic_async(
            &self.file,
            &serde_json::to_vec_pretty(config).map_err(ApiError::internal)?,
            0o600,
        )
        .await
        .map_err(ApiError::internal)
    }

    pub async fn manifest(&self, state: &AppState) -> ApiResult<Manifest> {
        let mut cached = self.manifest.lock().await;
        let revision = crate::catalog::meta_revision();
        if cached
            .as_ref()
            .is_none_or(|(previous, _)| *previous != revision)
        {
            let catalog = crate::catalog::read_catalog(&state.paths).ok_or_else(|| {
                ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "主题目录尚未生成")
            })?;
            let mut assets = BTreeMap::new();
            for theme in &catalog.themes {
                for path in std::iter::once(&theme.path)
                    .chain(std::iter::once(&theme.preview_path))
                    .chain(theme.avatar_path.iter())
                    .chain(theme.screenshot_paths.iter())
                {
                    if path.is_empty() || assets.contains_key(path) {
                        continue;
                    }
                    if !valid_asset_path(path) {
                        return Err(ApiError::internal("目录含有无效资源路径"));
                    }
                    let size = state.store.size(path).await;
                    if size == 0 || size > MAX_ASSET_BYTES {
                        return Err(ApiError::internal("目录资源大小无效"));
                    }
                    let sha256 = if path == &theme.path && valid_hash(&theme.sha256) {
                        theme.sha256.clone()
                    } else {
                        state.store.sha256(path).await.map_err(ApiError::internal)?
                    };
                    assets.insert(path.clone(), Asset { size, sha256 });
                }
            }
            let digest = hex::encode(Sha256::digest(
                serde_json::to_vec(&assets).map_err(ApiError::internal)?,
            ));
            *cached = Some((
                revision,
                Manifest {
                    uploads_enabled: false,
                    revision: digest,
                    assets,
                    maintenance: false,
                    poll_interval_secs: 5,
                    lease_secs: 15,
                    cache_ttl_secs: 60,
                },
            ));
        }
        let mut manifest = cached
            .as_ref()
            .ok_or_else(|| ApiError::internal("资源清单生成失败"))?
            .1
            .clone();
        let config = self.config.read().await;
        manifest.maintenance = state.maintenance.load(std::sync::atomic::Ordering::Relaxed);
        manifest.poll_interval_secs = config.sync_interval_secs;
        manifest.lease_secs = (config.sync_interval_secs * 3).max(15);
        manifest.cache_ttl_secs = config.cache_ttl_secs;
        manifest.uploads_enabled = config.uploads_enabled;
        Ok(manifest)
    }

    pub(crate) async fn authorize_node(&self, headers: &HeaderMap) -> ApiResult<Node> {
        let id = headers
            .get(NODE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let secret = headers
            .get(SECRET_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let config = self.config.read().await;
        config
            .nodes
            .iter()
            .find(|node| {
                node.enabled
                    && node.id == id
                    && node.secret.as_bytes().ct_eq(secret.as_bytes()).into()
            })
            .cloned()
            .filter(|_| config.enabled)
            .ok_or_else(|| ApiError::unauthorized("节点认证失败或分发已关闭"))
    }

    pub async fn probe(&self) -> Vec<Value> {
        let nodes = self.config.read().await.nodes.clone();
        futures::future::join_all(nodes.into_iter().map(|node| async move {
            let started = Instant::now();
            let status = if !node.enabled {
                json!({"ready": false, "state": "disabled"})
            } else {
                let response = self
                    .client
                    .get(endpoint(&node.base_url, "_edge/status").unwrap())
                    .header(SECRET_HEADER, &node.secret)
                    .send()
                    .await;
                match response {
                    Ok(response) if response.status().is_success() => {
                        match bounded_json::<Value>(response, 64 * 1024).await {
                            Ok(value) => value,
                            Err(_) => json!({"ready": false, "state": "invalid-response"}),
                        }
                    }
                    _ => json!({"ready": false, "state": "unreachable"}),
                }
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            self.probes.write().await.insert(
                node.id.clone(),
                Probe {
                    at: Instant::now(),
                    latency_ms,
                    status: status.clone(),
                },
            );
            json!({"id": node.id, "latencyMs": latency_ms, "status": status})
        }))
        .await
    }

    /// A bounded, parallel probe of the actual requested file. A stale/missing edge
    /// never wins merely because its health endpoint is fast.
    pub async fn pick(&self, state: &AppState, path: &str) -> Option<String> {
        let config = self.config.read().await.clone();
        if !config.enabled {
            return None;
        }
        let manifest = self.manifest(state).await.ok()?;
        if manifest.maintenance {
            return None;
        }
        let asset = manifest.assets.get(path)?;
        self.pick_asset(&config, path, asset).await
    }

    async fn pick_asset(
        &self,
        config: &DistributionConfig,
        path: &str,
        asset: &Asset,
    ) -> Option<String> {
        // Include node configuration in the key so a concurrent disable or key
        // rotation cannot resurrect a route calculated against the old config.
        let generation = hex::encode(Sha256::digest(serde_json::to_vec(config).ok()?));
        let cache_key = format!("{generation}|{path}|{}", asset.sha256);
        if let Some((at, id)) = self.routes.lock().await.get(&cache_key) {
            let ttl = if id.is_some() { 5 } else { 2 };
            if at.elapsed() < Duration::from_secs(ttl) {
                return id
                    .as_ref()
                    .and_then(|id| config.nodes.iter().find(|n| n.enabled && &n.id == id))
                    .and_then(|node| {
                        ticket_url(node, path, &asset.sha256, chrono::Utc::now().timestamp()).ok()
                    });
            }
        }
        let probes = self.probes.read().await;
        let mut nodes: Vec<_> = config.nodes.iter().filter(|node| node.enabled).collect();
        nodes.sort_by_key(|node| {
            let latency = probes
                .get(&node.id)
                .filter(|p| p.at.elapsed() < Duration::from_secs(30))
                .map(|p| p.latency_ms)
                .unwrap_or(u64::MAX);
            if config.strategy == "priority" {
                (node.priority as i64, latency)
            } else {
                (0, latency)
            }
        });
        drop(probes);
        let requests = nodes.iter().enumerate().map(|(index, node)| {
            let url = ticket_url(node, path, &asset.sha256, chrono::Utc::now().timestamp()).ok();
            async move {
                let url = url?;
                let started = Instant::now();
                let response = self.client.head(&url).send().await.ok()?;
                // Require a matching digest as well as a successful response.
                if response.status() != StatusCode::OK
                    || response
                        .headers()
                        .get("X-Sukisu-Asset-Sha256")
                        .and_then(|v| v.to_str().ok())
                        != Some(asset.sha256.as_str())
                {
                    return None;
                }
                Some((index, started.elapsed(), url))
            }
        });
        let available = futures::future::join_all(requests).await;
        let selected = available
            .into_iter()
            .flatten()
            .min_by_key(|(index, elapsed, _)| {
                if config.strategy == "priority" {
                    (*index as u128, 0)
                } else {
                    (elapsed.as_micros(), *index as u128)
                }
            });
        let mut routes = self.routes.lock().await;
        if routes.len() >= 4096 {
            routes.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(5));
        }
        if routes.len() >= 4096 {
            routes.clear();
        }
        routes.insert(
            cache_key,
            (
                Instant::now(),
                selected
                    .as_ref()
                    .map(|(index, _, _)| nodes[*index].id.clone()),
            ),
        );
        selected.map(|(_, _, url)| url)
    }
}

fn default_true() -> bool {
    true
}

pub fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_hexdigit() && !v.is_ascii_uppercase())
}

pub fn valid_asset_path(path: &str) -> bool {
    crate::util::normalize_asset_key(path).as_deref() == Some(path)
        && !path.starts_with("avatars/users/")
        && !path.contains(['%', '?', '#', ':'])
        && !path.chars().any(char::is_control)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// Private node credentials must only traverse TLS (loopback HTTP is useful for local testing).
pub fn normalize_url(value: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(value.trim())?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    anyhow::ensure!(
        url.host_str().is_some()
            && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "节点和主站地址必须使用 HTTPS，且不能包含账号、查询参数或片段（本机测试可用 HTTP）"
    );
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn endpoint(base: &str, path: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{}/", base.trim_end_matches('/')))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("无效节点地址"))?
        .pop_if_empty()
        .extend(path.split('/'));
    Ok(url)
}

fn validate_config(config: &DistributionConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(config.strategy.as_str(), "latency" | "priority"),
        "无效选路策略"
    );
    anyhow::ensure!(
        (2..=60).contains(&config.sync_interval_secs) && config.cache_ttl_secs <= 300,
        "同步间隔须为 2–60 秒，缓存有效期须为 0–300 秒"
    );
    if !config.origin_url.is_empty() {
        normalize_url(&config.origin_url)?;
    }
    anyhow::ensure!(
        !config.enabled || !config.origin_url.is_empty(),
        "请先配置主站 HTTPS 地址"
    );
    anyhow::ensure!(config.nodes.len() <= 16, "最多允许 16 个节点");
    let mut ids = std::collections::HashSet::new();
    let mut urls = std::collections::HashSet::new();
    for node in &config.nodes {
        anyhow::ensure!(
            valid_id(&node.id)
                && valid_hash(&node.secret)
                && !node.name.trim().is_empty()
                && node.name.chars().count() <= 48,
            "无效节点配置"
        );
        let url = normalize_url(&node.base_url)?;
        anyhow::ensure!(
            ids.insert(&node.id) && urls.insert(url.clone()),
            "节点 ID 或地址重复"
        );
        anyhow::ensure!(url != config.origin_url, "节点不能使用主站地址");
    }
    Ok(())
}

/// Reuse AES-GCM rather than inventing a MAC. The authenticated payload binds
/// a short-lived capability to one path AND one content digest.
fn ticket(secret: &str, path: &str, digest: &str, now: i64) -> anyhow::Result<String> {
    let key = hex::decode(secret)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("无效节点密钥"))?;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let payload = format!("{}\n{path}\n{digest}", now + TICKET_SECONDS);
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), payload.as_bytes())
        .map_err(|_| anyhow::anyhow!("下载凭证生成失败"))?;
    Ok(format!("{}.{}", hex::encode(nonce), hex::encode(encrypted)))
}

pub fn verify_ticket(secret: &str, token: &str, path: &str, now: i64) -> Option<(String, i64)> {
    if token.len() > 8192 || !valid_asset_path(path) {
        return None;
    }
    let (nonce, encrypted) = token.split_once('.')?;
    let nonce = hex::decode(nonce).ok()?;
    if nonce.len() != 12 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(&hex::decode(secret).ok()?).ok()?;
    let payload = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            hex::decode(encrypted).ok()?.as_ref(),
        )
        .ok()?;
    let payload = String::from_utf8(payload).ok()?;
    let parts: Vec<_> = payload.split('\n').collect();
    if parts.len() != 3 || parts[1] != path || !valid_hash(parts[2]) {
        return None;
    }
    let expires = parts[0].parse::<i64>().ok()?;
    if expires <= now || expires > now + TICKET_SECONDS {
        return None;
    }
    Some((parts[2].to_string(), expires))
}

fn ticket_url(node: &Node, path: &str, digest: &str, now: i64) -> anyhow::Result<String> {
    let mut url = endpoint(&node.base_url, path)?;
    url.query_pairs_mut()
        .append_pair("ticket", &ticket(&node.secret, path, digest, now)?);
    Ok(url.to_string())
}

#[cfg(test)]
pub fn test_ticket_url(node: &Node, path: &str, digest: &str) -> String {
    ticket_url(node, path, digest, chrono::Utc::now().timestamp()).unwrap()
}

pub async fn bounded_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    max: usize,
) -> anyhow::Result<T> {
    use futures::StreamExt;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        anyhow::ensure!(bytes.len() + chunk.len() <= max, "节点响应超出限制");
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/distribution", get(admin_get).post(admin_save))
        .route("/distribution/probe", post(admin_probe))
        .route("/distribution/nodes", post(node_save))
        .route(
            "/distribution/nodes/{id}",
            axum::routing::delete(node_delete),
        )
        .route("/distribution/nodes/{id}/rotate", post(node_rotate))
        .route("/distribution/nodes/{id}/sync", post(node_sync))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}

pub fn origin_router() -> Router<AppState> {
    Router::new()
        .route("/distribution/manifest", get(origin_manifest))
        .route("/distribution/asset", get(origin_asset))
        .route(
            "/distribution/upload/receipt",
            post(crate::resumable::edge_receipt)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
}

async fn admin_get(State(state): State<AppState>) -> ApiResult<Response> {
    let config = state.distribution.config.read().await;
    let probes = state.distribution.probes.read().await;
    let nodes: Vec<_> = config
        .nodes
        .iter()
        .map(|node| {
            let probe = probes
                .get(&node.id)
                .filter(|p| p.at.elapsed() < Duration::from_secs(30));
            json!({"id": node.id, "name": node.name, "baseUrl": node.base_url,
            "enabled": node.enabled, "uploadEnabled": node.upload_enabled, "priority": node.priority,
            "latencyMs": probe.map(|p| p.latency_ms), "status": probe.map(|p| &p.status)})
        })
        .collect();
    no_store(
        json!({"ok": true, "enabled": config.enabled, "originUrl": config.origin_url,
        "uploadsEnabled": config.uploads_enabled,
        "strategy": config.strategy, "syncIntervalSecs": config.sync_interval_secs,
        "cacheTtlSecs": config.cache_ttl_secs, "nodes": nodes,
        "database": {"mode": "primary-only", "layout": state.paths.db_layout.as_str(),
            "replicated": false, "files": ["account.db", "authors.db", "social.db", "stats.db", "audit.db"]}}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsInput {
    enabled: bool,
    #[serde(default = "default_true")]
    uploads_enabled: bool,
    origin_url: String,
    strategy: String,
    sync_interval_secs: u64,
    cache_ttl_secs: u64,
}

async fn admin_save(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Json(input): Json<SettingsInput>,
) -> ApiResult<Response> {
    let mut guard = state.distribution.config.write().await;
    let mut config = guard.clone();
    config.enabled = input.enabled;
    config.uploads_enabled = input.uploads_enabled;
    config.origin_url = if input.origin_url.trim().is_empty() {
        String::new()
    } else {
        normalize_url(&input.origin_url).map_err(|e| ApiError::bad(e.to_string()))?
    };
    config.strategy = input.strategy;
    config.sync_interval_secs = input.sync_interval_secs;
    config.cache_ttl_secs = input.cache_ttl_secs;
    validate_config(&config).map_err(|e| ApiError::bad(e.to_string()))?;
    state.distribution.persist(&config).await?;
    *guard = config;
    drop(guard);
    state.distribution.probes.write().await.clear();
    audit(&state, &actor, "distribution.settings", None).await;
    no_store(json!({"ok": true}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NodeInput {
    #[serde(default)]
    id: String,
    name: String,
    base_url: String,
    enabled: bool,
    #[serde(default = "default_true")]
    upload_enabled: bool,
    priority: i32,
}

async fn node_save(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Json(input): Json<NodeInput>,
) -> ApiResult<Response> {
    let mut guard = state.distribution.config.write().await;
    let mut config = guard.clone();
    let created = input.id.is_empty();
    let id = if created {
        crate::util::random_hex(8)
    } else {
        input.id
    };
    let base_url = normalize_url(&input.base_url).map_err(|e| ApiError::bad(e.to_string()))?;
    let secret = if created {
        crate::util::random_hex(32)
    } else {
        config
            .nodes
            .iter()
            .find(|n| n.id == id)
            .ok_or_else(|| ApiError::not_found("节点不存在"))?
            .secret
            .clone()
    };
    let node = Node {
        id: id.clone(),
        name: input.name.trim().into(),
        base_url,
        enabled: input.enabled,
        upload_enabled: input.upload_enabled,
        priority: input.priority,
        secret,
    };
    config.nodes.retain(|n| n.id != id);
    config.nodes.push(node.clone());
    validate_config(&config).map_err(|e| ApiError::bad(e.to_string()))?;
    if created && config.origin_url.is_empty() {
        return Err(ApiError::bad("请先保存主站 HTTPS 地址"));
    }
    let edge_config = created.then(|| crate::edge::EdgeConfig::for_node(&node, &config.origin_url));
    state.distribution.persist(&config).await?;
    *guard = config;
    drop(guard);
    state.distribution.probes.write().await.remove(&id);
    audit(&state, &actor, "distribution.node.save", Some(&id)).await;
    no_store(json!({"ok": true, "id": id, "edgeConfig": edge_config}))
}

async fn node_delete(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // The shared legacy CSRF middleware checks POST only.
    if headers
        .get(crate::config::CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        != Some(crate::config::CSRF_VALUE)
    {
        return Err(ApiError::forbidden("missing request header"));
    }
    let mut guard = state.distribution.config.write().await;
    let mut config = guard.clone();
    if !config.nodes.iter().any(|n| n.id == id) {
        return Err(ApiError::not_found("节点不存在"));
    }
    config.nodes.retain(|n| n.id != id);
    state.distribution.persist(&config).await?;
    *guard = config;
    drop(guard);
    state.distribution.probes.write().await.remove(&id);
    audit(&state, &actor, "distribution.node.delete", Some(&id)).await;
    no_store(json!({"ok": true}))
}

async fn node_rotate(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let mut guard = state.distribution.config.write().await;
    let mut config = guard.clone();
    let node = config
        .nodes
        .iter_mut()
        .find(|n| n.id == id)
        .ok_or_else(|| ApiError::not_found("节点不存在"))?;
    node.secret = crate::util::random_hex(32);
    let edge_config = crate::edge::EdgeConfig::for_node(node, &config.origin_url);
    state.distribution.persist(&config).await?;
    *guard = config;
    drop(guard);
    state.distribution.probes.write().await.remove(&id);
    audit(&state, &actor, "distribution.node.rotate", Some(&id)).await;
    no_store(json!({"ok": true, "edgeConfig": edge_config}))
}

async fn admin_probe(State(state): State<AppState>) -> ApiResult<Response> {
    no_store(json!({"ok": true, "nodes": state.distribution.probe().await}))
}

async fn node_sync(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let node = state
        .distribution
        .config
        .read()
        .await
        .nodes
        .iter()
        .find(|n| n.id == id && n.enabled)
        .cloned()
        .ok_or_else(|| ApiError::not_found("节点不存在或已禁用"))?;
    let response = state
        .distribution
        .client
        .post(endpoint(&node.base_url, "_edge/sync").map_err(ApiError::internal)?)
        .header(SECRET_HEADER, &node.secret)
        .send()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "节点不可达"))?;
    if !response.status().is_success() {
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "节点拒绝同步请求"));
    }
    audit(&state, &actor, "distribution.node.sync", Some(&id)).await;
    no_store(json!({"ok": true, "message": "已请求节点同步，请刷新查看实际进度"}))
}

async fn origin_manifest(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let node = state.distribution.authorize_node(&headers).await?;
    let mut manifest = state.distribution.manifest(&state).await?;
    manifest.uploads_enabled &= node.upload_enabled;
    let bytes = serde_json::to_vec(&manifest).map_err(ApiError::internal)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ApiError::internal("同步清单超过大小限制"));
    }
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(&bytes)));
    let unchanged =
        headers.get("If-None-Match").and_then(|v| v.to_str().ok()) == Some(etag.as_str());
    Response::builder()
        .status(if unchanged {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header("ETag", etag)
        .header("Cache-Control", "no-store")
        .header("Content-Type", "application/json")
        .body(if unchanged {
            axum::body::Body::empty()
        } else {
            axum::body::Body::from(bytes)
        })
        .map_err(ApiError::internal)
}

#[derive(Deserialize)]
struct AssetQuery {
    path: String,
    sha256: String,
}

async fn origin_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AssetQuery>,
) -> ApiResult<Response> {
    state.distribution.authorize_node(&headers).await?;
    let manifest = state.distribution.manifest(&state).await?;
    if manifest.maintenance {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "站点维护中"));
    }
    let asset = manifest
        .assets
        .get(&query.path)
        .filter(|a| a.sha256 == query.sha256)
        .ok_or_else(|| ApiError::not_found("资源未发布或已更新"))?;
    let object = state
        .store
        .serve(&query.path, None)
        .await
        .map_err(|e| match e {
            ServeError::NotFound => ApiError::not_found("资源不存在"),
            _ => ApiError::internal("读取资源失败"),
        })?;
    if object.content_length != asset.size {
        return Err(ApiError::conflict("资源正在更新"));
    }
    crate::api::serve_object_response(object, "no-store", state.download_buffer_bytes())
}

pub fn no_store(value: Value) -> ApiResult<Response> {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert("Cache-Control", "no-store".parse().unwrap());
    Ok(response)
}

/// Call only after the entry point's existing token/session/ownership checks.
pub async fn redirect_asset(
    state: &AppState,
    path: &str,
    count_download: bool,
) -> ApiResult<Option<Response>> {
    let Some(location) = state.distribution.pick(state, path).await else {
        return Ok(None);
    };
    if count_download {
        if let Some(theme_id) = crate::catalog::theme_id_from_package_path(path) {
            state.record_theme_download(&theme_id);
        }
    }
    Ok(Some(
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", location)
            .header("Cache-Control", "no-store")
            .header("Referrer-Policy", "no-referrer")
            .body(axum::body::Body::empty())
            .map_err(ApiError::internal)?,
    ))
}

async fn audit(state: &AppState, actor: &SessionInfo, action: &str, id: Option<&str>) {
    if let Err(error) = crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        action,
        Some("distribution"),
        id,
        None,
    )
    .await
    {
        tracing::warn!(?error, "distribution audit failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_bind_path_hash_expiry_and_key() {
        let key = "ab".repeat(32);
        let hash = "cd".repeat(32);
        let token = ticket(&key, "themes/a.sukisu", &hash, 1000).unwrap();
        assert_eq!(
            verify_ticket(&key, &token, "themes/a.sukisu", 1001),
            Some((hash, 1120))
        );
        assert!(verify_ticket(&key, &token, "themes/b.sukisu", 1001).is_none());
        assert!(verify_ticket(&"ef".repeat(32), &token, "themes/a.sukisu", 1001).is_none());
        assert!(verify_ticket(&key, &token, "themes/a.sukisu", 1120).is_none());
        assert!(verify_ticket(&key, &format!("{token}00"), "themes/a.sukisu", 1001).is_none());
    }
    #[test]
    fn rejects_unsafe_addresses_and_paths() {
        for url in [
            "http://edge.example",
            "https://user:pass@edge.example",
            "https://edge.example/?key=x",
            "file:///tmp",
        ] {
            assert!(normalize_url(url).is_err());
        }
        assert!(normalize_url("https://edge.example:8443/base/").is_ok());
        assert!(normalize_url("http://127.0.0.1:8081").is_ok());
        for path in [
            "pending/a",
            "themes/../store.env",
            "themes/%2e%2e/a",
            "themes/a?x=1",
            "/themes/a",
            "themes/a\nb",
        ] {
            assert!(!valid_asset_path(path));
        }
        assert!(valid_asset_path("themes/_loc/nas/a-v1.sukisu"));
    }
    #[test]
    fn encodes_paths_and_preserves_base_path() {
        assert_eq!(
            endpoint("https://edge.example/cache", "themes/a b.sukisu")
                .unwrap()
                .as_str(),
            "https://edge.example/cache/themes/a%20b.sukisu"
        );
    }

    #[tokio::test]
    async fn automatic_routing_checks_file_digest_and_falls_back_when_edges_fail() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hash = "cd".repeat(32);
        let digest = hash.clone();
        let app = Router::new().route(
            "/themes/a.sukisu",
            get(move || {
                let digest = digest.clone();
                async move { ([("X-Sukisu-Asset-Sha256", digest)], "package") }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = DistributionConfig {
            enabled: true,
            origin_url: "https://primary.example".into(),
            nodes: vec![Node {
                id: "edge".into(),
                name: "edge".into(),
                base_url: format!("http://{address}"),
                secret: "ab".repeat(32),
                enabled: true,
                upload_enabled: true,
                priority: 100,
            }],
            ..Default::default()
        };
        let pool = Distribution {
            file: "/unused".into(),
            config: RwLock::new(config.clone()),
            manifest: Mutex::new(None),
            probes: RwLock::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                .unwrap(),
        };
        let asset = Asset {
            size: 7,
            sha256: hash.clone(),
        };
        assert!(pool
            .pick_asset(&config, "themes/a.sukisu", &asset)
            .await
            .unwrap()
            .starts_with(&format!("http://{address}")));
        assert!(pool
            .pick_asset(&config, "themes/missing.sukisu", &asset)
            .await
            .is_none());
        let wrong = Asset {
            size: 7,
            sha256: "ef".repeat(32),
        };
        assert!(pool
            .pick_asset(&config, "themes/a.sukisu", &wrong)
            .await
            .is_none());
        let mut disabled = config.clone();
        disabled.nodes[0].enabled = false;
        assert!(pool
            .pick_asset(&disabled, "themes/a.sukisu", &asset)
            .await
            .is_none());
        pool.routes.lock().await.clear();
        task.abort();
        assert!(pool
            .pick_asset(&config, "themes/a.sukisu", &asset)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn node_config_persists_privately_and_never_reveals_secret_in_probe_status() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("distribution.json");
        let node = Node {
            id: "edge".into(),
            name: "edge".into(),
            base_url: "http://127.0.0.1:1".into(),
            secret: "ab".repeat(32),
            enabled: false,
            priority: 100,
            upload_enabled: true,
        };
        let config = DistributionConfig {
            nodes: vec![node.clone()],
            ..Default::default()
        };
        validate_config(&config).unwrap();
        let pool = Distribution {
            file: file.clone(),
            config: RwLock::new(config.clone()),
            manifest: Mutex::new(None),
            probes: RwLock::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            client: reqwest::Client::new(),
        };
        pool.persist(&config).await.unwrap();
        let loaded: DistributionConfig =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(loaded.nodes[0].secret, node.secret);
        assert!(!serde_json::to_string(&pool.probe().await)
            .unwrap()
            .contains(&node.secret));
        assert!(pool.authorize_node(&HeaderMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn routing_defaults_to_measured_response_time_and_supports_priority_override() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fast_hash = "cd".repeat(32);
        let slow_hash = fast_hash.clone();
        let app = Router::new()
            .route(
                "/fast/themes/a.sukisu",
                get(move || {
                    let hash = fast_hash.clone();
                    async move { ([("X-Sukisu-Asset-Sha256", hash)], "package") }
                }),
            )
            .route(
                "/slow/themes/a.sukisu",
                get(move || {
                    let hash = slow_hash.clone();
                    async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        ([("X-Sukisu-Asset-Sha256", hash)], "package")
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = DistributionConfig {
            enabled: true,
            origin_url: "https://primary.example".into(),
            nodes: vec![
                Node {
                    id: "slow".into(),
                    name: "slow".into(),
                    base_url: format!("http://{address}/slow"),
                    secret: "ab".repeat(32),
                    enabled: true,
                    upload_enabled: true,
                    priority: 1,
                },
                Node {
                    id: "fast".into(),
                    name: "fast".into(),
                    base_url: format!("http://{address}/fast"),
                    secret: "ab".repeat(32),
                    enabled: true,
                    upload_enabled: true,
                    priority: 100,
                },
            ],
            ..Default::default()
        };
        let pool = Distribution {
            file: "/unused".into(),
            config: RwLock::new(config.clone()),
            manifest: Mutex::new(None),
            probes: RwLock::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            client: reqwest::Client::new(),
        };
        let asset = Asset {
            size: 7,
            sha256: "cd".repeat(32),
        };
        assert!(pool
            .pick_asset(&config, "themes/a.sukisu", &asset)
            .await
            .unwrap()
            .contains("/fast/themes/"));
        config.strategy = "priority".into();
        assert!(pool
            .pick_asset(&config, "themes/a.sukisu", &asset)
            .await
            .unwrap()
            .contains("/slow/themes/"));
        task.abort();
    }
}
