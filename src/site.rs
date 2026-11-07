//! Complete website nodes with one explicitly transferred writer. Followers
//! proxy business operations, run their own edge cache, and retain private
//! checkpoints. Only a final checkpoint taken after the old writer exits can
//! be activated. There is deliberately no timeout-based promotion.
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio_util::io::ReaderStream;

use crate::api::{ApiError, AppState};
use crate::config::{self, Paths};
use crate::distribution::{self, DistributionConfig, Node};
use crate::util::{random_hex, unix_now};

const KEY: &str = "X-Sukisu-Site-Key";
const HOP: &str = "X-Sukisu-Site-Hop";
const CHUNK: usize = 1024 * 1024;
const DBS: [&str; 5] = [
    "account.db",
    "authors.db",
    "social.db",
    "stats.db",
    "audit.db",
];

static DRAINING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static STOP: tokio::sync::Notify = tokio::sync::Notify::const_new();
static BACKGROUND: RwLock<()> = RwLock::const_new(());

pub async fn background() -> Option<tokio::sync::RwLockReadGuard<'static, ()>> {
    let guard = BACKGROUND.read().await;
    if DRAINING.load(std::sync::atomic::Ordering::SeqCst) {
        None
    } else {
        Some(guard)
    }
}
pub async fn shutdown_child() {
    STOP.notified().await;
}
/// Survives supervisor crashes: a replacement child cannot open the databases
/// until the orphan has drained its outstanding requests and background jobs.
pub fn lock_child() -> Result<Option<std::fs::File>> {
    let Some(lease) = std::env::var_os("SUKISU_SITE_LEASE") else {
        return Ok(None);
    };
    Ok(Some(lock_writer(
        Path::new(&lease).with_file_name("core.lock").as_path(),
    )?))
}
fn lock_writer(path: &Path) -> Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    anyhow::ensure!(
        fs4::fs_std::FileExt::try_lock_exclusive(&lock)?,
        "原业务进程尚未退出，禁止同时打开写入服务"
    );
    Ok(lock)
}
fn child_lease_valid() -> bool {
    let Some(file) = std::env::var_os("SUKISU_SITE_LEASE") else {
        return true;
    };
    let expected = std::env::var("SUKISU_SITE_INTERNAL").unwrap_or_default();
    let lease = std::fs::read(file)
        .ok()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        .unwrap_or(Value::Null);
    !expected.is_empty()
        && lease["mode"] == "writer"
        && lease["expiresAt"].as_u64().unwrap_or(0) > unix_now()
        && lease["keyHash"] == hex::encode(Sha256::digest(&expected))
}
pub fn watch_child(state: AppState) {
    if std::env::var_os("SUKISU_SITE_LEASE").is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !child_lease_valid() {
                DRAINING.store(true, std::sync::atomic::Ordering::SeqCst);
                let _background = BACKGROUND.write().await;
                while state.jobs.is_running() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                state.flush_downloads().await;
                STOP.notify_one();
                break;
            }
        }
    });
}
pub async fn drain_child(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let expected = std::env::var("SUKISU_SITE_INTERNAL").unwrap_or_default();
    let provided = headers
        .get("X-Sukisu-Site-Internal")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if expected.is_empty() || !crate::api::token_matches(&expected, provided) {
        return StatusCode::NOT_FOUND.into_response();
    }
    DRAINING.store(true, std::sync::atomic::Ordering::SeqCst);
    let _background = BACKGROUND.write().await;
    while state.jobs.is_running() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    state.flush_downloads().await;
    STOP.notify_one();
    Json(json!({"ok":true})).into_response()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub edge_secret: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Seed {
    pub root: PathBuf,
    pub db_dir: PathBuf,
    pub files: BTreeMap<String, PathBuf>,
}

impl Seed {
    fn from_paths(paths: &Paths) -> Self {
        Self {
            root: paths.root.clone(),
            db_dir: paths.db_dir.clone(),
            files: [
                ("config.env", paths.admin_config.clone()),
                ("account.env", paths.account_env.clone()),
                ("admin.env", paths.admin_env.clone()),
                ("admin-password", paths.admin_password_file.clone()),
                ("authors.json", paths.authors_file.clone()),
                ("storage.env", paths.storage_env.clone()),
                ("sources.json", paths.sources_file.clone()),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect(),
        }
    }
    fn for_dir(dir: &Path) -> Self {
        Self {
            root: dir.join("store"),
            db_dir: dir.join("db"),
            files: [
                "config.env",
                "account.env",
                "admin.env",
                "admin-password",
                "authors.json",
                "storage.env",
                "sources.json",
            ]
            .into_iter()
            .map(|k| (k.into(), dir.join("config").join(k)))
            .collect(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SiteConfig {
    pub cluster_id: String,
    pub node_id: String,
    pub writer_id: String,
    pub epoch: u64,
    pub mode: String,
    pub secret: String,
    pub peers: Vec<Peer>,
    pub bind: std::net::SocketAddr,
    pub data_dir: PathBuf,
    pub seed: Option<Seed>,
    pub active_dir: Option<PathBuf>,
    #[serde(default = "sync_default")]
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub transfer_id: Option<String>,
    #[serde(default)]
    pub final_snapshot: Option<String>,
    #[serde(default)]
    pub transfer_actor: Option<String>,
    #[serde(default = "quota_default")]
    pub quota_bytes: u64,
}
fn sync_default() -> u64 {
    60
}
fn quota_default() -> u64 {
    20 * 1024 * 1024 * 1024
}

fn directory_bytes(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        anyhow::ensure!(!entry.file_type().is_symlink(), "站点目录包含符号链接");
        if entry.file_type().is_file() {
            total = total
                .checked_add(entry.metadata()?.len())
                .context("目录大小溢出")?;
        }
    }
    Ok(total)
}
fn capacity(c: &SiteConfig, additional: u64) -> Result<()> {
    let budget = c.quota_bytes.saturating_mul(6);
    anyhow::ensure!(
        directory_bytes(&c.data_dir)?.saturating_add(additional) <= budget,
        "站点工作目录超过总预算（单份配额的六倍）；暂停同步，保留活动数据"
    );
    anyhow::ensure!(
        fs4::available_space(&c.data_dir)? > additional.saturating_add(64 * 1024 * 1024),
        "磁盘剩余空间不足，暂停同步"
    );
    Ok(())
}
fn prune(c: &SiteConfig) -> Result<()> {
    use std::collections::HashSet;
    private_dir(&c.data_dir)?;
    let mut manifests = Vec::new();
    let snapshots = c.data_dir.join("snapshots");
    if snapshots.is_dir() {
        for entry in std::fs::read_dir(&snapshots)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&std::fs::read(entry.path())?)
            else {
                continue;
            };
            if snapshot.cluster_id == c.cluster_id
                && snapshot_file(c, &snapshot.id)? == entry.path()
            {
                manifests.push((snapshot, entry.path()));
            }
        }
    }
    manifests.sort_by_key(|(s, _)| std::cmp::Reverse(s.created_at));
    let mut keep = HashSet::new();
    for (index, (snapshot, file)) in manifests.into_iter().enumerate() {
        if index < 2 || c.final_snapshot.as_deref() == Some(&snapshot.id) {
            for f in snapshot.files.values() {
                keep.extend(f.chunks.iter().cloned());
            }
        } else {
            std::fs::remove_file(file)?;
        }
    }
    let replicas = c.data_dir.join("replicas");
    let mut dirs = Vec::new();
    if replicas.is_dir() {
        for entry in std::fs::read_dir(&replicas)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path().join("complete.json")) else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes) else {
                continue;
            };
            if snapshot.cluster_id == c.cluster_id
                && snapshot_file(c, &snapshot.id).is_ok()
                && entry.file_name().to_str() == Some(&snapshot.id)
            {
                dirs.push((snapshot, entry.path()));
            }
        }
    }
    dirs.sort_by_key(|(s, _)| std::cmp::Reverse(s.created_at));
    for (index, (snapshot, dir)) in dirs.into_iter().enumerate() {
        if index < 2
            || c.active_dir.as_ref() == Some(&dir)
            || c.final_snapshot.as_deref() == Some(&snapshot.id)
        {
            for f in snapshot.files.values() {
                keep.extend(f.chunks.iter().cloned());
            }
        } else {
            // Remove only complete generated replicas within the locked, dedicated directory.
            directory_bytes(&dir)?;
            std::fs::remove_dir_all(dir)?;
        }
    }
    let objects = c.data_dir.join("objects");
    if objects.is_dir() {
        for entry in std::fs::read_dir(objects)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if entry.file_type()?.is_file()
                && distribution::valid_hash(name)
                && !keep.contains(name)
            {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

impl SiteConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            distribution::valid_hash(&self.secret) && self.epoch > 0,
            "站点集群身份无效"
        );
        anyhow::ensure!(
            (2..=16).contains(&self.peers.len()) && self.data_dir.is_absolute(),
            "需要 2–16 个节点及绝对数据目录"
        );
        anyhow::ensure!(self.bind.ip().is_loopback(), "站点服务只允许回环监听");
        anyhow::ensure!(
            ["writer", "follower", "fenced"].contains(&self.mode.as_str()),
            "站点角色无效"
        );
        anyhow::ensure!(
            self.peers.iter().any(|p| p.id == self.node_id)
                && self.peers.iter().any(|p| p.id == self.writer_id),
            "节点或写入节点不在成员列表"
        );
        anyhow::ensure!(
            self.mode != "writer" || self.writer_id == self.node_id,
            "写入身份不匹配"
        );
        anyhow::ensure!(
            (10..=3600).contains(&self.sync_interval_secs) && self.quota_bytes >= 256 * 1024 * 1024,
            "同步间隔或配额无效"
        );
        let mut ids = std::collections::HashSet::new();
        let mut urls = std::collections::HashSet::new();
        for p in &self.peers {
            anyhow::ensure!(
                !p.id.is_empty()
                    && p.id.len() <= 64
                    && p.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "无效节点 ID"
            );
            let url = distribution::normalize_url(&p.base_url)?;
            anyhow::ensure!(
                ids.insert(&p.id) && urls.insert(url) && distribution::valid_hash(&p.edge_secret),
                "重复节点或无效边缘密钥"
            );
        }
        if let Some(dir) = &self.active_dir {
            let relative = dir
                .strip_prefix(self.data_dir.join("replicas"))
                .context("活动副本必须位于专用副本目录")?;
            anyhow::ensure!(
                !relative.as_os_str().is_empty()
                    && relative
                        .components()
                        .all(|c| matches!(c, Component::Normal(_))),
                "活动副本路径无效"
            );
        }
        if let Some(seed) = &self.seed {
            anyhow::ensure!(
                seed.root.is_absolute()
                    && seed.db_dir.is_absolute()
                    && !self.data_dir.starts_with(&seed.root)
                    && !seed.root.starts_with(&self.data_dir)
                    && !self.data_dir.starts_with(&seed.db_dir)
                    && !seed.db_dir.starts_with(&self.data_dir),
                "站点工作目录不能位于原资源目录内部"
            );
        }
        Ok(())
    }
    fn source(&self) -> Result<Seed> {
        if let Some(dir) = &self.active_dir {
            return Ok(Seed::for_dir(dir));
        }
        self.seed.clone().context("本节点尚无可激活的数据副本")
    }
    fn peer(&self, id: &str) -> Result<&Peer> {
        self.peers.iter().find(|p| p.id == id).context("未知节点")
    }
    fn url(&self) -> Result<&str> {
        Ok(&self.peer(&self.node_id)?.base_url)
    }
}

fn private_dir(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        if let Ok(m) = std::fs::symlink_metadata(parent) {
            anyhow::ensure!(!m.file_type().is_symlink(), "站点目录不接受符号链接");
        }
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    anyhow::ensure!(
        !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()),
        "拒绝符号链接目标"
    );
    let parent = path.parent().context("无效配置路径")?;
    if parent.exists() {
        for ancestor in parent.ancestors() {
            anyhow::ensure!(
                !std::fs::symlink_metadata(ancestor)?
                    .file_type()
                    .is_symlink(),
                "站点目录不接受符号链接"
            );
        }
    } else {
        private_dir(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", random_hex(8)));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    drop(output);
    std::fs::rename(&temp, path)?;
    #[cfg(unix)]
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn persist(path: &Path, value: &SiteConfig) -> Result<()> {
    atomic(path, &serde_json::to_vec_pretty(value)?)
}

/// Update the supervisor listener from the network page. The supervisor reads
/// this file at startup, so the caller must restart the site service.
pub fn update_bind(bind: std::net::SocketAddr) -> Result<bool> {
    let file = std::env::var_os("SUKISU_SITE_CONFIG").context("当前不是完整站点进程")?;
    let path = PathBuf::from(file);
    let bytes = std::fs::read(&path)?;
    let mut config: SiteConfig = serde_json::from_slice(&bytes)?;
    if config.bind == bind {
        return Ok(false);
    }
    anyhow::ensure!(bind.ip().is_loopback(), "完整站点只允许回环监听地址");
    config.bind = bind;
    persist(&path, &config)?;
    Ok(true)
}

pub fn public_endpoints(paths: &Paths) -> Value {
    if let Some(file) = std::env::var_os("SUKISU_SITE_CONFIG") {
        if let Ok(bytes) = std::fs::read(file) {
            if let Ok(c) = serde_json::from_slice::<SiteConfig>(&bytes) {
                return json!(c.peers.iter().map(|p|json!({"id":p.id,"name":p.name,"baseUrl":p.base_url,"writer":p.id==c.writer_id,
                    "catalogUrl":format!("{}/catalog.json",p.base_url.trim_end_matches('/')),"healthUrl":format!("{}/site-health",p.base_url.trim_end_matches('/'))})).collect::<Vec<_>>());
            }
        }
    }
    let base = config::read_network_settings(paths).public_url;
    json!([{"id":"local","name":"当前站点","baseUrl":base,"writer":true,"catalogUrl":format!("{}/catalog.json",base.trim_end_matches('/')),"healthUrl":format!("{}/healthz",base.trim_end_matches('/'))}])
}

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/sites", get(unmanaged))
        .route("/sites/bootstrap", post(bootstrap))
}
async fn unmanaged(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"ok":true,"managed":false,"endpoints":public_endpoints(&state.paths)}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    nodes: Vec<BootstrapNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapNode {
    name: String,
    base_url: String,
    data_dir: PathBuf,
}

async fn bootstrap(
    State(state): State<AppState>,
    Json(body): Json<Bootstrap>,
) -> Result<Response, ApiError> {
    if body.nodes.len() < 2 || body.nodes.len() > 16 {
        return Err(ApiError::bad("需要 2–16 个站点，第一项是当前写入节点"));
    }
    let source = std::env::var_os("SUKISU_SITE_CONFIG")
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|v| serde_json::from_slice::<SiteConfig>(&v).ok())
        .and_then(|c| c.seed);
    let backend = source
        .as_ref()
        .and_then(|s| s.files.get("storage.env"))
        .map(|p| config::read_storage_config(p).backend)
        .unwrap_or(state.settings.backend);
    let has_remote_sources = source
        .as_ref()
        .and_then(|s| s.files.get("sources.json"))
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        .and_then(|v| v.get("sources").and_then(Value::as_array).cloned())
        .is_some_and(|v| !v.is_empty());
    if backend != config::Backend::Local
        || has_remote_sources
        || !state.sources.list().await.is_empty()
    {
        return Err(ApiError::bad(
            "完整站点切换目前要求本地主存储且未配置额外存储；请先迁移资源，不能遗漏 NAS/S3 数据",
        ));
    }
    let mut dirs = std::collections::HashSet::new();
    for node in &body.nodes {
        if !node.data_dir.is_absolute()
            || !dirs.insert(node.data_dir.clone())
            || node.name.trim().is_empty()
        {
            return Err(ApiError::bad("节点需要名称和独立的绝对私有目录"));
        }
    }
    let peers = body
        .nodes
        .iter()
        .map(|n| {
            Ok(Peer {
                id: format!("site-{}", random_hex(6)),
                name: n.name.chars().take(64).collect(),
                base_url: distribution::normalize_url(&n.base_url)?,
                edge_secret: random_hex(32),
            })
        })
        .collect::<Result<Vec<_>>>()
        .map_err(ApiError::internal)?;
    let secret = random_hex(32);
    let cluster = format!("cluster-{}", random_hex(12));
    let mut configs = Vec::new();
    for (i, n) in body.nodes.iter().enumerate() {
        let c = SiteConfig {
            cluster_id: cluster.clone(),
            node_id: peers[i].id.clone(),
            writer_id: peers[0].id.clone(),
            epoch: 1,
            mode: if i == 0 { "writer" } else { "follower" }.into(),
            secret: secret.clone(),
            peers: peers.clone(),
            bind: "127.0.0.1:8080".parse().unwrap(),
            data_dir: n.data_dir.clone(),
            seed: if i == 0 {
                Some(Seed::from_paths(&state.paths))
            } else {
                None
            },
            active_dir: None,
            sync_interval_secs: 60,
            transfer_id: None,
            final_snapshot: None,
            transfer_actor: None,
            quota_bytes: quota_default(),
        };
        c.validate().map_err(ApiError::internal)?;
        configs.push(json!({"name":n.name,"config":c}));
    }
    Ok(([("Cache-Control", "no-store"), ("Referrer-Policy", "no-referrer")], Json(
        json!({"ok":true,"configs":configs,"message":"配置仅显示一次，含节点内部密钥。保存到各站点后以 site 模式启动，不会自动部署或修改现有服务。"}),
    )).into_response())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileRef {
    size: u64,
    chunks: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    id: String,
    cluster_id: String,
    source_id: String,
    epoch: u64,
    final_copy: bool,
    created_at: u64,
    files: BTreeMap<String, FileRef>,
}
fn safe_file(value: &str) -> bool {
    !value.contains('\\')
        && !value.contains(':')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value
            .split('/')
            .any(|v| v.is_empty() || v == "." || v == "..")
        && value.len() < 1024
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && (value.starts_with("store/")
            || DBS.iter().any(|n| value == format!("db/{n}"))
            || [
                "config/config.env",
                "config/account.env",
                "config/admin.env",
                "config/admin-password",
                "config/authors.json",
                "config/storage.env",
                "config/sources.json",
            ]
            .contains(&value))
}
fn snapshot_file(c: &SiteConfig, id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "无效副本 ID"
    );
    Ok(c.data_dir.join("snapshots").join(format!("{id}.json")))
}
fn add_file(
    c: &SiteConfig,
    files: &mut BTreeMap<String, FileRef>,
    name: String,
    path: &Path,
    total: &mut u64,
) -> Result<()> {
    use std::io::Read;
    anyhow::ensure!(
        safe_file(&name) && files.len() < 200000,
        "文件路径或数量超出限制"
    );
    let mut input = std::fs::File::open(path)?;
    let mut chunks = Vec::new();
    let mut size = 0;
    loop {
        let mut buffer = vec![0; CHUNK];
        let mut count = 0;
        while count < CHUNK {
            let n = input.read(&mut buffer[count..])?;
            if n == 0 {
                break;
            }
            count += n;
        }
        if count == 0 {
            break;
        }
        buffer.truncate(count);
        size += count as u64;
        anyhow::ensure!(
            total.saturating_add(size) <= c.quota_bytes,
            "完整副本超过站点配额"
        );
        let hash = hex::encode(Sha256::digest(&buffer));
        let object = c.data_dir.join("objects").join(&hash);
        if !object.exists() {
            anyhow::ensure!(
                fs4::available_space(&c.data_dir)? > count as u64 + 64 * 1024 * 1024,
                "站点磁盘空间不足"
            );
            atomic(&object, &buffer)?;
        }
        chunks.push(hash);
    }
    *total += size;
    anyhow::ensure!(*total <= c.quota_bytes, "完整副本超过站点配额");
    files.insert(name, FileRef { size, chunks });
    Ok(())
}
fn walk(
    c: &SiteConfig,
    root: &Path,
    current: &Path,
    files: &mut BTreeMap<String, FileRef>,
    total: &mut u64,
) -> Result<()> {
    if !current.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        anyhow::ensure!(
            !ty.is_symlink(),
            "完整副本不接受符号链接；请使用独立本地存储"
        );
        if ty.is_dir() {
            walk(c, root, &entry.path(), files, total)?;
        } else if ty.is_file() {
            let name = format!(
                "store/{}",
                entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/")
            );
            add_file(c, files, name, &entry.path(), total)?;
        }
    }
    Ok(())
}

fn checkpoint(c: &SiteConfig, final_copy: bool) -> Result<Snapshot> {
    let mut source = c.source()?;
    let runtime_config = c.data_dir.join("runtime/config.env");
    if runtime_config.is_file() {
        source.files.insert("config.env".into(), runtime_config);
    }
    prune(c)?;
    let estimate = directory_bytes(&source.root)?.saturating_add(directory_bytes(&source.db_dir)?);
    anyhow::ensure!(estimate <= c.quota_bytes, "完整源数据超过站点单份配额");
    capacity(c, estimate.saturating_mul(2))?;
    let id = random_hex(16);
    let mut files = BTreeMap::new();
    let mut total = 0;
    let work = c.data_dir.join("snapshot-work").join(&id);
    private_dir(&work)?;
    for name in DBS {
        let conn = rusqlite::Connection::open_with_flags(
            source.db_dir.join(name),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(Duration::from_secs(30))?;
        let destination = work.join(name);
        conn.backup(rusqlite::DatabaseName::Main, &destination, None)?;
        add_file(
            c,
            &mut files,
            format!("db/{name}"),
            &destination,
            &mut total,
        )?;
    }
    walk(c, &source.root, &source.root, &mut files, &mut total)?;
    for (name, path) in &source.files {
        if path.is_file() {
            add_file(c, &mut files, format!("config/{name}"), path, &mut total)?;
        }
    }
    let snapshot = Snapshot {
        id,
        cluster_id: c.cluster_id.clone(),
        source_id: c.node_id.clone(),
        epoch: c.epoch,
        final_copy,
        created_at: unix_now(),
        files,
    };
    atomic(
        &snapshot_file(c, &snapshot.id)?,
        &serde_json::to_vec(&snapshot)?,
    )?;
    // Scratch backups belong to this checkpoint; active data is never replaced.
    std::fs::remove_dir_all(work)?;
    Ok(snapshot)
}

struct Children {
    core: Option<Child>,
    core_url: Option<String>,
    edge: Option<Child>,
    edge_url: Option<String>,
    core_key: String,
}
struct Host {
    file: PathBuf,
    config: RwLock<SiteConfig>,
    client: reqwest::Client,
    children: Mutex<Children>,
    gate: Arc<RwLock<()>>,
    operation: Mutex<()>,
    snapshot: RwLock<Option<Snapshot>>,
    replica: RwLock<Option<Snapshot>>,
    error: RwLock<Option<String>>,
}

fn authenticated(c: &SiteConfig, headers: &HeaderMap) -> bool {
    headers
        .get(KEY)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            s.len() == c.secret.len() && bool::from(s.as_bytes().ct_eq(c.secret.as_bytes()))
        })
}
fn endpoint(c: &SiteConfig, id: &str, path: &str) -> Result<reqwest::Url> {
    distribution::endpoint(&c.peer(id)?.base_url, path)
}
fn state_json(c: &SiteConfig, ready: bool) -> Value {
    json!({"ok":true,"managed":true,"clusterId":c.cluster_id,"nodeId":c.node_id,"writerId":c.writer_id,"epoch":c.epoch,"mode":c.mode,"ready":ready,"transferId":c.transfer_id,"finalSnapshot":c.final_snapshot})
}
fn unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"ok":false,"error":message})),
    )
        .into_response()
}

impl Host {
    async fn core_ready(&self) -> bool {
        let mut children = self.children.lock().await;
        let alive = children
            .core
            .as_mut()
            .is_some_and(|c| c.try_wait().ok().flatten().is_none());
        if !alive {
            children.core = None;
            children.core_url = None;
        }
        alive
    }
    async fn resume_transfer(&self) -> Result<()> {
        let mut c = self.config.read().await.clone();
        anyhow::ensure!(c.mode == "fenced", "本节点没有未完成交接");
        if c.final_snapshot.is_none() {
            let snapshot = self.make_checkpoint(true).await?;
            c.final_snapshot = Some(snapshot.id);
            c.epoch = c.epoch.checked_add(1).context("代次溢出")?;
            self.save(c.clone()).await?;
        }
        let reply=self.client.post(endpoint(&c,&c.writer_id,"_site/activate")?).header(KEY,&c.secret)
            .json(&json!({"sourceId":c.node_id,"epoch":c.epoch,"snapshotId":c.final_snapshot,"transferId":c.transfer_id}))
            .timeout(Duration::from_secs(1800)).send().await?;
        anyhow::ensure!(
            reply.status().is_success(),
            "交接尚未完成；旧节点保持停止写入"
        );
        c.mode = "follower".into();
        c.transfer_actor = None;
        self.save(c).await?;
        self.start_edge().await?;
        Ok(())
    }
    async fn follow_writer(&self) -> Result<()> {
        let mut c = self.config.read().await.clone();
        let previous = self.peer_json(&c, &c.writer_id, "_site/state").await?;
        if previous["epoch"].as_u64() == Some(c.epoch) && previous["writerId"] == c.writer_id {
            return Ok(());
        }
        let writer = previous["writerId"].as_str().context("缺少写入节点")?;
        anyhow::ensure!(
            previous["mode"] == "follower" && previous["epoch"].as_u64() == Some(c.epoch + 1),
            "缺少原写入节点的交接证明"
        );
        let next = self.peer_json(&c, writer, "_site/state").await?;
        anyhow::ensure!(
            next["mode"] == "writer"
                && next["writerId"] == writer
                && next["epoch"] == previous["epoch"]
                && next["transferId"] == previous["transferId"]
                && next["finalSnapshot"] == previous["finalSnapshot"],
            "写入节点交接证明不一致"
        );
        self.stop_edge().await?;
        c.writer_id = writer.into();
        c.epoch += 1;
        c.transfer_id = next["transferId"].as_str().map(str::to_string);
        c.final_snapshot = next["finalSnapshot"].as_str().map(str::to_string);
        self.save(c).await?;
        self.start_edge().await
    }
    async fn save(&self, c: SiteConfig) -> Result<()> {
        let mut current = self.config.write().await;
        persist(&self.file, &c)?;
        *current = c;
        Ok(())
    }
    async fn peer_json(&self, c: &SiteConfig, id: &str, path: &str) -> Result<Value> {
        distribution::bounded_json(
            self.client
                .get(endpoint(c, id, path)?)
                .header(KEY, &c.secret)
                .timeout(Duration::from_secs(10))
                .send()
                .await?
                .error_for_status()?,
            32 * 1024 * 1024,
        )
        .await
    }
    async fn core_request(&self, request: Request) -> Result<Response> {
        let children = self.children.lock().await;
        let base = children.core_url.clone().context("写入服务尚未启动")?;
        let key = children.core_key.clone();
        drop(children);
        self.forward(request, &base, Some(&key), false).await
    }
    async fn forward(
        &self,
        request: Request,
        base: &str,
        key: Option<&str>,
        peer: bool,
    ) -> Result<Response> {
        let (parts, body) = request.into_parts();
        let suffix = parts
            .uri
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or("/");
        let url = format!("{}{}", base.trim_end_matches('/'), suffix);
        let mut headers = parts.headers;
        for name in [
            "host",
            "connection",
            "content-length",
            "transfer-encoding",
            "keep-alive",
            "upgrade",
            "proxy-authorization",
            KEY,
            "X-Sukisu-Site-Internal",
            HOP,
        ] {
            headers.remove(name);
        }
        if let Some(key) = key {
            headers.insert("X-Sukisu-Site-Internal", key.parse()?);
        }
        if peer {
            headers.insert(HOP, "1".parse()?);
            let public = {
                let config = self.config.read().await;
                config.url().ok().map(str::to_string)
            };
            if let Some(public) = public {
                if let Ok(value) = public.parse() {
                    headers.insert("x-sukisu-forwarded-site", value);
                }
            }
        }
        let upstream = self
            .client
            .request(parts.method, url)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await?;
        let status = upstream.status();
        let mut headers = upstream.headers().clone();
        for name in ["connection", "transfer-encoding", "keep-alive", "upgrade"] {
            headers.remove(name);
        }
        let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }
    async fn admin(&self, headers: &HeaderMap) -> Result<()> {
        let c = self.config.read().await.clone();
        if c.mode == "fenced" {
            let token = crate::auth::session_token(headers, crate::auth::ADMIN_SESSION_COOKIE)
                .unwrap_or_default();
            anyhow::ensure!(
                c.transfer_actor.as_deref() == Some(&crate::accounts::hash_token(&token)),
                "交接恢复需要原管理员会话"
            );
            return Ok(());
        }
        let (url, key) = if c.mode == "writer" {
            let ch = self.children.lock().await;
            (
                format!(
                    "{}/api/session",
                    ch.core_url.as_ref().context("写入服务未启动")?
                ),
                Some(ch.core_key.clone()),
            )
        } else {
            (endpoint(&c, &c.writer_id, "api/session")?.to_string(), None)
        };
        let mut r = self.client.get(url).timeout(Duration::from_secs(10));
        for name in ["cookie", "authorization"] {
            if let Some(v) = headers.get(name) {
                r = r.header(name, v);
            }
        }
        if let Some(k) = key {
            r = r.header("X-Sukisu-Site-Internal", k);
        }
        let result: Value =
            distribution::bounded_json(r.send().await?.error_for_status()?, 16384).await?;
        anyhow::ensure!(
            result["authenticated"].as_bool() == Some(true)
                && result["role"].as_str() == Some("admin"),
            "请先登录管理员账号"
        );
        Ok(())
    }
    async fn start_core(&self) -> Result<()> {
        anyhow::ensure!(!self.core_ready().await, "写入进程已在运行");
        let c = self.config.read().await.clone();
        anyhow::ensure!(c.mode == "writer", "非写入节点不能启动业务写入服务");
        let source = c.source()?;
        let source_storage = source.files.get("storage.env").context("缺少原存储配置")?;
        anyhow::ensure!(
            config::read_storage_config(source_storage).backend == config::Backend::Local,
            "完整站点目前只支持本地存储"
        );
        if let Some(file) = source.files.get("sources.json").filter(|p| p.exists()) {
            let value: Value = serde_json::from_slice(&std::fs::read(file)?)?;
            anyhow::ensure!(
                value["sources"].as_array().is_none_or(Vec::is_empty),
                "完整站点不允许额外存储来源"
            );
        }
        private_dir(&c.data_dir.join("runtime"))?;
        let runtime = c.data_dir.join("runtime");
        let storage = runtime.join("storage.env");
        let network = runtime.join("network.env");
        let port = free_port().await?;
        let core_base = if c.bind.is_ipv4() {
            format!("http://127.0.0.1:{port}")
        } else {
            format!("http://[::1]:{port}")
        };
        atomic(
            &storage,
            format!(
                "THEME_STORAGE_ROOT={}\nTHEME_STORAGE_LIMIT_BYTES={}\n",
                source.root.to_string_lossy().replace('\\', "/"),
                c.quota_bytes
            )
            .as_bytes(),
        )?;
        let mut network_values = config::read_env(&network);
        let loopback = if c.bind.is_ipv4() { "127.0.0.1" } else { "::1" };
        network_values.insert("ADMIN_BIND".to_string(), loopback.to_string());
        network_values.insert("ADMIN_PORT".to_string(), port.to_string());
        network_values
            .entry("OUTBOUND_IP_FAMILY".to_string())
            .or_insert_with(|| "auto".to_string());
        atomic(
            &network,
            network_values
                .into_iter()
                .map(|(key, value)| format!("{key}={value}\n"))
                .collect::<String>()
                .as_bytes(),
        )?;
        // The source config remains at its original location; only the selected
        // node's local PUBLIC_URL/DB directory are adjusted in a private runtime copy.
        let config_file = runtime.join("config.env");
        let config_exists = config_file.exists();
        let mut values = config::read_env(if config_exists {
            &config_file
        } else {
            source.files.get("config.env").context("缺少站点配置")?
        });
        values.insert("THEME_DB_LAYOUT".into(), "split".into());
        values.insert(
            "THEME_DB_DIR".into(),
            source.db_dir.to_string_lossy().replace('\\', "/"),
        );
        let public_url = c.url()?.to_string();
        let admin_url = format!("{}/admin/", public_url.trim_end_matches('/'));
        if config_exists {
            // Keep values customised on the network page across child restarts.
            if values
                .get("PUBLIC_URL")
                .is_none_or(|value| value.trim().is_empty())
            {
                values.insert("PUBLIC_URL".into(), public_url);
            }
            if values
                .get("ADMIN_URL")
                .is_none_or(|value| value.trim().is_empty())
            {
                values.insert("ADMIN_URL".into(), admin_url);
            }
        } else {
            values.insert("PUBLIC_URL".into(), public_url);
            values.insert("ADMIN_URL".into(), admin_url);
        }
        atomic(
            &config_file,
            values
                .into_iter()
                .map(|(k, v)| format!("{k}={v}\n"))
                .collect::<String>()
                .as_bytes(),
        )?;
        let distribution_file = runtime.join("distribution.json");
        let dist = DistributionConfig {
            enabled: true,
            uploads_enabled: true,
            origin_url: c.url()?.into(),
            strategy: "latency".into(),
            sync_interval_secs: 5,
            cache_ttl_secs: 60,
            nodes: c
                .peers
                .iter()
                .filter(|p| p.id != c.node_id)
                .map(|p| Node {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    base_url: p.base_url.clone(),
                    secret: p.edge_secret.clone(),
                    enabled: true,
                    upload_enabled: true,
                    priority: 100,
                })
                .collect(),
        };
        atomic(&distribution_file, &serde_json::to_vec(&dist)?)?;
        let core_key = random_hex(32);
        let lease = runtime.join("lease.json");
        write_lease(&c, &lease, &core_key)?;
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.kill_on_drop(true)
            .env("THEME_STORAGE_ENV", storage)
            .env("THEME_ADMIN_CONFIG", config_file)
            .env("THEME_NETWORK_ENV", network)
            .env(
                "THEME_ACCOUNT_ENV",
                source
                    .files
                    .get("account.env")
                    .context("缺少账号配置路径")?,
            )
            .env(
                "THEME_ADMIN_ENV",
                source
                    .files
                    .get("admin.env")
                    .context("缺少管理员配置路径")?,
            )
            .env(
                "THEME_ADMIN_PASSWORD_FILE",
                source
                    .files
                    .get("admin-password")
                    .context("缺少管理员密码路径")?,
            )
            .env(
                "THEME_AUTHORS_FILE",
                source
                    .files
                    .get("authors.json")
                    .context("缺少作者配置路径")?,
            )
            .env("THEME_DB_PATH", runtime.join("unused-legacy.db"))
            .env("THEME_SOURCES_FILE", runtime.join("empty-sources.json"))
            .env("THEME_DISTRIBUTION_FILE", distribution_file)
            .env("SUKISU_SITE_CONFIG", &self.file)
            .env("SUKISU_SITE_LEASE", &lease)
            .env("SUKISU_SITE_INTERNAL", &core_key)
            .env("THEME_DISABLE_SYSTEMCTL", "1");
        let child = cmd.spawn()?;
        {
            let mut children = self.children.lock().await;
            children.core = Some(child);
            children.core_url = Some(core_base.clone());
            children.core_key = core_key;
        }
        for _ in 0..100 {
            let key = self.children.lock().await.core_key.clone();
            if self
                .client
                .get(format!("{core_base}/healthz"))
                .header("X-Sukisu-Site-Internal", key)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("写入服务启动失败，节点保持关闭写入，请检查日志")
    }
    async fn stop_core(&self) -> Result<()> {
        let mut children = self.children.lock().await;
        if let Some(child) = children.core.as_mut() {
            let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
                .await
                .context("业务进程尚未退出，禁止交接")??;
            anyhow::ensure!(status.success(), "业务进程异常退出，禁止激活最终副本");
        }
        children.core = None;
        children.core_url = None;
        Ok(())
    }
    async fn start_edge(&self) -> Result<()> {
        let c = self.config.read().await.clone();
        if c.mode == "writer" {
            return Ok(());
        }
        let mut children = self.children.lock().await;
        if children.edge.is_some() {
            return Ok(());
        }
        let me = c.peer(&c.node_id)?;
        let port = free_port().await?;
        let edge = crate::edge::EdgeConfig {
            node_id: c.node_id.clone(),
            secret: me.edge_secret.clone(),
            origin_url: c.peer(&c.writer_id)?.base_url.clone(),
            cache_dir: c.data_dir.join("edge-cache"),
            bind: format!("127.0.0.1:{port}").parse()?,
            quota_bytes: c.quota_bytes,
            upload_quota_bytes: 1024 * 1024 * 1024,
        };
        let file = c.data_dir.join("runtime/edge.json");
        atomic(&file, &serde_json::to_vec(&edge)?)?;
        let child = Command::new(std::env::current_exe()?)
            .arg("edge")
            .arg(file)
            .kill_on_drop(true)
            .env_remove("SUKISU_SITE_INTERNAL")
            .env_remove("SUKISU_SITE_LEASE")
            .spawn()?;
        children.edge = Some(child);
        children.edge_url = Some(format!("http://127.0.0.1:{port}"));
        Ok(())
    }
    async fn stop_edge(&self) -> Result<()> {
        let mut ch = self.children.lock().await;
        if let Some(mut child) = ch.edge.take() {
            child.kill().await?;
            child.wait().await?;
        }
        ch.edge_url = None;
        Ok(())
    }

    async fn make_checkpoint(&self, final_copy: bool) -> Result<Snapshot> {
        let c = self.config.read().await.clone();
        let state_dir = c.data_dir.clone();
        let snapshot = tokio::task::spawn_blocking(move || checkpoint(&c, final_copy)).await??;
        atomic(
            &state_dir.join("latest.json"),
            &serde_json::to_vec(&snapshot)?,
        )?;
        *self.snapshot.write().await = Some(snapshot.clone());
        Ok(snapshot)
    }

    async fn sync_from(
        &self,
        source_id: &str,
        id: &str,
        final_required: bool,
    ) -> Result<(Snapshot, PathBuf)> {
        let c = self.config.read().await.clone();
        let value = self
            .peer_json(&c, source_id, &format!("_site/snapshots/{id}"))
            .await?;
        let snapshot: Snapshot = serde_json::from_value(value)?;
        anyhow::ensure!(
            snapshot.cluster_id == c.cluster_id
                && snapshot.source_id == source_id
                && snapshot.id == id
                && (!final_required || snapshot.final_copy),
            "副本身份或交接标记不匹配"
        );
        anyhow::ensure!(snapshot.files.len() <= 200000, "副本文件数量超限");
        let bytes: u64 = snapshot
            .files
            .values()
            .map(|f| f.size)
            .try_fold(0u64, |n, v| n.checked_add(v).context("副本大小溢出"))?;
        anyhow::ensure!(bytes <= c.quota_bytes, "副本超过本节点配额");
        prune(&c)?;
        capacity(&c, bytes.saturating_mul(2))?;
        snapshot_file(&c, id)?;
        let destination = c.data_dir.join("replicas").join(id);
        private_dir(&destination)?;
        anyhow::ensure!(
            c.active_dir.as_ref() != Some(&destination),
            "禁止覆盖正在使用的数据库副本"
        );
        for (name, file) in &snapshot.files {
            anyhow::ensure!(
                safe_file(name) && file.chunks.len() as u64 == file.size.div_ceil(CHUNK as u64),
                "副本路径或分块数量无效"
            );
            let mut output = tokio::fs::File::create(destination.join(".file-building")).await?;
            let mut written = 0u64;
            for hash in &file.chunks {
                anyhow::ensure!(distribution::valid_hash(hash), "分块摘要无效");
                let object = c.data_dir.join("objects").join(hash);
                let cached = tokio::fs::read(&object)
                    .await
                    .ok()
                    .filter(|v| v.len() <= CHUNK && hex::encode(Sha256::digest(v)) == *hash);
                let buffer = if let Some(value) = cached {
                    value
                } else {
                    let response = self
                        .client
                        .get(endpoint(&c, source_id, &format!("_site/objects/{hash}"))?)
                        .header(KEY, &c.secret)
                        .send()
                        .await?
                        .error_for_status()?;
                    let mut stream = response.bytes_stream();
                    let mut data = Vec::new();
                    while let Some(bytes) = stream.try_next().await? {
                        anyhow::ensure!(data.len() + bytes.len() <= CHUNK, "分块过大");
                        data.extend_from_slice(&bytes);
                    }
                    anyhow::ensure!(
                        hex::encode(Sha256::digest(&data)) == *hash,
                        "同步分块校验失败"
                    );
                    atomic(&object, &data)?;
                    data
                };
                written += buffer.len() as u64;
                output.write_all(&buffer).await?;
            }
            anyhow::ensure!(written == file.size, "同步文件长度不符");
            output.sync_all().await?;
            drop(output);
            let target = destination.join(name);
            private_dir(target.parent().unwrap())?;
            tokio::fs::rename(destination.join(".file-building"), target).await?;
        }
        for name in DBS {
            let file = destination.join("db").join(name);
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let conn = rusqlite::Connection::open_with_flags(
                    file,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                let valid: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
                anyhow::ensure!(valid == "ok", "副本数据库完整性检查失败");
                Ok(())
            })
            .await?;
            result?;
        }
        atomic(
            &destination.join("complete.json"),
            &serde_json::to_vec(&snapshot)?,
        )?;
        atomic(
            &c.data_dir.join("replica.json"),
            &serde_json::to_vec(&snapshot)?,
        )?;
        *self.replica.write().await = Some(snapshot.clone());
        Ok((snapshot, destination))
    }
}

async fn free_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}
fn write_lease(c: &SiteConfig, path: &Path, key: &str) -> Result<()> {
    atomic(
        path,
        &serde_json::to_vec(
            &json!({"mode":c.mode,"epoch":c.epoch,"keyHash":hex::encode(Sha256::digest(key)),"expiresAt":unix_now()+10}),
        )?,
    )
}

/// Managed children accept traffic only from their supervisor while its durable
/// writer lease is valid. A stale/orphan child cannot regain write admission.
pub async fn child_guard(request: Request, next: axum::middleware::Next) -> Response {
    if let Some(file) = std::env::var_os("SUKISU_SITE_LEASE") {
        let expected = std::env::var("SUKISU_SITE_INTERNAL").unwrap_or_default();
        let provided = request
            .headers()
            .get("X-Sukisu-Site-Internal")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let lease = std::fs::read(file)
            .ok()
            .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
            .unwrap_or(Value::Null);
        if provided.len() != expected.len()
            || !bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
            || lease["mode"] != "writer"
            || lease["expiresAt"].as_u64().unwrap_or(0) <= unix_now()
            || lease["keyHash"] != hex::encode(Sha256::digest(&expected))
        {
            return unavailable("节点没有有效写入租约");
        }
    }
    next.run(request).await
}

async fn host_state(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    let c = host.config.read().await.clone();
    if !authenticated(&c, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ready = host.core_ready().await;
    Json(state_json(&c, ready)).into_response()
}
async fn health(State(host): State<Arc<Host>>) -> Json<Value> {
    let c = host.config.read().await;
    Json(json!({"ok":true,"nodeId":c.node_id,"writerId":c.writer_id,"mode":c.mode,"epoch":c.epoch}))
}
async fn shutdown_host(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    if !authenticated(&*host.config.read().await, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _operation = host.operation.lock().await;
    let _gate = host.gate.write().await;
    if host.core_ready().await {
        let ch = host.children.lock().await;
        let base = ch.core_url.clone().unwrap();
        let key = ch.core_key.clone();
        drop(ch);
        match host
            .client
            .post(format!("{base}/_site/drain"))
            .header("X-Sukisu-Site-Internal", key)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            _ => return unavailable("业务进程未完成排空"),
        }
        if let Err(e) = host.stop_core().await {
            return unavailable(&e.to_string());
        }
    }
    if let Err(e) = host.stop_edge().await {
        return unavailable(&e.to_string());
    }
    STOP.notify_one();
    Json(json!({"ok":true})).into_response()
}
async fn snapshot_get(
    State(host): State<Arc<Host>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let c = host.config.read().await.clone();
    if !authenticated(&c, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match snapshot_file(&c, &id).and_then(|p| std::fs::read(p).map_err(Into::into)) {
        Ok(v) => (
            [
                ("Content-Type", "application/json"),
                ("Cache-Control", "no-store"),
            ],
            v,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn object_get(
    State(host): State<Arc<Host>>,
    AxumPath(hash): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let c = host.config.read().await.clone();
    if !authenticated(&c, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !distribution::valid_hash(&hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match tokio::fs::File::open(c.data_dir.join("objects").join(hash)).await {
        Ok(file) => Response::builder()
            .header("Cache-Control", "no-store")
            .body(Body::from_stream(ReaderStream::new(file)))
            .unwrap(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn sites(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    if host.admin(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let c = host.config.read().await.clone();
    let mut nodes = Vec::new();
    for peer in &c.peers {
        let value = if peer.id == c.node_id {
            Ok(state_json(
                &c,
                host.children.lock().await.core_url.is_some(),
            ))
        } else {
            host.peer_json(&c, &peer.id, "_site/state").await
        };
        nodes.push(json!({"id":peer.id,"name":peer.name,"baseUrl":peer.base_url,"reachable":value.is_ok(),"state":value.ok()}));
    }
    Json(json!({"ok":true,"managed":true,"nodeId":c.node_id,"writerId":c.writer_id,"epoch":c.epoch,"mode":c.mode,"nodes":nodes,"syncIntervalSecs":c.sync_interval_secs,"quotaBytes":c.quota_bytes,
        "snapshot":host.snapshot.read().await.as_ref().map(|s|json!({"id":s.id,"createdAt":s.created_at,"final":s.final_copy,"files":s.files.len()})),
        "replica":host.replica.read().await.as_ref().map(|s|json!({"id":s.id,"createdAt":s.created_at,"files":s.files.len()})),"error":*host.error.read().await})).into_response()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalSettings {
    sync_interval_secs: u64,
    quota_bytes: u64,
}
async fn local_settings(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Json(body): Json<LocalSettings>,
) -> Response {
    if !csrf(&headers) || host.admin(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _operation = host.operation.lock().await;
    let mut c = host.config.read().await.clone();
    if !(10..=3600).contains(&body.sync_interval_secs)
        || body.quota_bytes < 256 * 1024 * 1024
        || body.quota_bytes > 10 * 1024u64.pow(4)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    c.sync_interval_secs = body.sync_interval_secs;
    c.quota_bytes = body.quota_bytes;
    match host.save(c).await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => unavailable(&e.to_string()),
    }
}
fn csrf(headers: &HeaderMap) -> bool {
    headers
        .get(config::CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(config::CSRF_VALUE)
}

async fn sync_now(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    if !csrf(&headers) || host.admin(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _operation = host.operation.lock().await;
    let c = host.config.read().await.clone();
    let result = if c.mode == "writer" {
        host.make_checkpoint(false).await.map(|_| ())
    } else {
        match host.peer_json(&c, &c.writer_id, "_site/latest").await {
            Ok(v) => match v["snapshotId"].as_str() {
                Some(id) => host.sync_from(&c.writer_id, id, false).await.map(|_| ()),
                None => Err(anyhow::anyhow!("写入节点尚无副本")),
            },
            Err(e) => Err(e),
        }
    };
    match result {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => {
            *host.error.write().await = Some(e.to_string());
            unavailable(&e.to_string())
        }
    }
}
async fn latest(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    let c = host.config.read().await.clone();
    if !authenticated(&c, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({"ok":true,"snapshotId":host.snapshot.read().await.as_ref().map(|s|&s.id)}))
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Transfer {
    target_id: String,
}
async fn transfer(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Json(body): Json<Transfer>,
) -> Response {
    if !csrf(&headers) || host.admin(&headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _operation = host.operation.lock().await;
    let mut c = host.config.read().await.clone();
    if c.mode == "fenced" && c.writer_id == body.target_id {
        return match host.resume_transfer().await {
            Ok(()) => Json(json!({"ok":true})).into_response(),
            Err(e) => unavailable(&e.to_string()),
        };
    }
    if c.mode != "writer"
        || c.writer_id != c.node_id
        || body.target_id == c.node_id
        || c.peer(&body.target_id).is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok":false,"error":"只能由当前写入节点发起交接"})),
        )
            .into_response();
    }
    let result:Result<()>=async {
        let peer=host.peer_json(&c,&body.target_id,"_site/state").await?;
        anyhow::ensure!(peer["clusterId"]==c.cluster_id && peer["writerId"]==c.writer_id && peer["epoch"].as_u64()==Some(c.epoch) && peer["mode"]=="follower","目标节点身份或代次不一致");
        for member in c.peers.iter().filter(|p|p.id!=c.node_id){
            let state=host.peer_json(&c,&member.id,"_site/state").await?;
            anyhow::ensure!(state["clusterId"]==c.cluster_id && state["epoch"].as_u64()==Some(c.epoch) && state["writerId"]==c.writer_id && state["mode"]=="follower","所有站点必须先确认当前代次，不能跳过未跟随完成的成员");
        }
        let _gate=host.gate.write().await;
        let children=host.children.lock().await;let base=children.core_url.clone().context("写入进程不可用")?;let key=children.core_key.clone();drop(children);
        let drained=host.client.post(format!("{base}/_site/drain")).header("X-Sukisu-Site-Internal",&key).timeout(Duration::from_secs(1800)).send().await?;
        anyhow::ensure!(drained.status().is_success(),"业务服务未完成排空，取消节点交接");
        // Existing proxied response streams hold the read gate through body completion.
        host.stop_core().await?;
        c.mode="fenced".into();c.writer_id=body.target_id.clone();c.transfer_id=Some(random_hex(16));c.final_snapshot=None;
        c.transfer_actor=crate::auth::session_token(&headers,crate::auth::ADMIN_SESSION_COOKIE).map(|v|crate::accounts::hash_token(&v));
        host.save(c.clone()).await?;
        let snapshot=host.make_checkpoint(true).await?;
        c.epoch=c.epoch.checked_add(1).context("代次溢出")?;c.final_snapshot=Some(snapshot.id.clone());
        host.save(c.clone()).await?;
        // The durable fence is written before the activation request. A crash or
        // lost reply therefore never restarts the old writer automatically.
        let lease=c.data_dir.join("runtime/lease.json");write_lease(&c,&lease,&key)?;
        let payload=json!({"sourceId":c.node_id,"epoch":c.epoch,"snapshotId":snapshot.id,"transferId":c.transfer_id});
        let reply=host.client.post(endpoint(&c,&body.target_id,"_site/activate")?).header(KEY,&c.secret).json(&payload).timeout(Duration::from_secs(1800)).send().await?;
        anyhow::ensure!(reply.status().is_success(),"目标节点未完成接管，旧节点保持停止写入；请查看目标日志并重试交接恢复");
        c.mode="follower".into();c.transfer_actor=None;host.save(c).await?;host.start_edge().await?;Ok(())
    }.await;
    match result {
        Ok(()) => Json(json!({"ok":true,"message":"写入节点已切换"})).into_response(),
        Err(e) => {
            *host.error.write().await = Some(e.to_string());
            unavailable(&e.to_string())
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Activate {
    source_id: String,
    epoch: u64,
    snapshot_id: String,
    transfer_id: String,
}
async fn activate(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Json(body): Json<Activate>,
) -> Response {
    let c = host.config.read().await.clone();
    if !authenticated(&c, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _operation = host.operation.lock().await;
    let _gate = host.gate.write().await;
    let result: Result<()> = async {
        let mut c = host.config.read().await.clone();
        if c.mode == "writer"
            && c.epoch == body.epoch
            && c.transfer_id.as_deref() == Some(&body.transfer_id)
        {
            if !host.core_ready().await {
                host.start_core().await?;
            }
            return Ok(());
        }
        anyhow::ensure!(
            c.mode == "follower" && c.writer_id == body.source_id && body.epoch == c.epoch + 1,
            "不接受过期、跳代或非当前写入节点的接管请求"
        );
        let source = host.peer_json(&c, &body.source_id, "_site/state").await?;
        anyhow::ensure!(
            source["mode"] == "fenced"
                && source["ready"] == false
                && source["writerId"] == c.node_id
                && source["epoch"].as_u64() == Some(body.epoch)
                && source["finalSnapshot"] == body.snapshot_id
                && source["transferId"] == body.transfer_id,
            "旧节点尚未持久隔离或交接凭据不匹配"
        );
        let (snapshot, directory) = host
            .sync_from(&body.source_id, &body.snapshot_id, true)
            .await?;
        anyhow::ensure!(snapshot.epoch == c.epoch, "最终副本代次不匹配");
        host.stop_edge().await?;
        c.active_dir = Some(directory);
        c.seed = None;
        c.writer_id = c.node_id.clone();
        c.epoch = body.epoch;
        c.mode = "writer".into();
        c.transfer_id = Some(body.transfer_id);
        c.final_snapshot = Some(body.snapshot_id);
        host.save(c.clone()).await?;
        if let Some(dir) = &c.active_dir {
            let source = dir.join("config/config.env");
            if source.exists() {
                atomic(
                    &c.data_dir.join("runtime/config.env"),
                    &std::fs::read(source)?,
                )?;
            }
        }
        host.start_core().await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => {
            *host.error.write().await = Some(e.to_string());
            unavailable(&e.to_string())
        }
    }
}

fn local_static(path: &str) -> Option<Response> {
    if matches!(
        path,
        "/" | "/index.html" | "/admin/login" | "/admin/login.html"
    ) {
        return Some(
            (
                StatusCode::FOUND,
                [
                    ("Location", "/account/login"),
                    ("Cache-Control", "no-store"),
                ],
            )
                .into_response(),
        );
    }
    let path = path.strip_prefix("/admin/").unwrap_or(path);
    let relative = match path {
        "" | "/admin" | "/admin/" => "index.html",
        "/account" | "/account/" | "/account/index.html" => "account.html",
        "/account/login" | "/account/register" | "/account/reset" | "/account/forgot" => {
            "account-auth.html"
        }
        "/account/device" | "/account/device/" => "account-device.html",
        "/account/logo.svg" => "logo.svg",
        "setup" | "setup.html" => "setup.html",
        other => other.trim_start_matches('/'),
    };
    let (bytes, content_type) = crate::api::embedded_static(relative)?;
    let disk = std::env::var_os("THEME_STATIC_DIR")
        .and_then(|p| std::fs::read(PathBuf::from(p).join(relative)).ok());
    let mut response = Response::new(Body::from(disk.unwrap_or_else(|| bytes.to_vec())));
    response
        .headers_mut()
        .insert("Content-Type", content_type.parse().ok()?);
    response
        .headers_mut()
        .insert("Cache-Control", "no-store".parse().unwrap());
    Some(response)
}

async fn proxy(State(host): State<Arc<Host>>, mut request: Request) -> Response {
    let guard = host.gate.clone().read_owned().await;
    let c = host.config.read().await.clone();
    let path = request.uri().path().to_string();
    // A peer tells us which public site the browser used so verification and
    // password-reset emails can link back to that node instead of the writer.
    let forwarded_site = request
        .headers()
        .get("x-sukisu-forwarded-site")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    request.headers_mut().remove("x-sukisu-site-public");
    request.headers_mut().remove("x-sukisu-forwarded-site");
    if c.mode == "writer" && request.headers().contains_key(HOP) {
        if let Some(public) = forwarded_site {
            if c.peers
                .iter()
                .any(|peer| peer.base_url.trim_end_matches('/') == public.trim_end_matches('/'))
            {
                if let Ok(value) = public.parse() {
                    request
                        .headers_mut()
                        .insert("x-sukisu-site-public", value);
                }
            }
        }
    }
    if path.starts_with("/_site/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(response) = local_static(&path) {
        return response;
    }
    if request.method() != axum::http::Method::GET
        && (path.ends_with("/storage/backend")
            || path.ends_with("/storage/root")
            || path.ends_with("/storage/sources/save")
            || path.ends_with("/storage/sources/import")
            || path.ends_with("/network"))
    {
        return (StatusCode::CONFLICT,Json(json!({"ok":false,"error":"完整站点模式请在本机站点设置中调整节点；存储布局和集群地址需停止服务后统一迁移"}))).into_response();
    }
    let result = if c.mode != "writer"
        && (path.starts_with("/_edge/")
            || (request
                .uri()
                .query()
                .is_some_and(|q| q.split('&').any(|v| v.starts_with("ticket=")))
                && config::ASSET_PREFIXES
                    .iter()
                    .any(|p| path.trim_start_matches('/').starts_with(p))))
    {
        match host.children.lock().await.edge_url.clone() {
            Some(base) => host.forward(request, &base, None, false).await,
            None => Err(anyhow::anyhow!("边缘不可用")),
        }
    } else if c.mode != "writer" && request.headers().contains_key(HOP) {
        return unavailable("写入节点正在交接，请稍后重试");
    } else if c.mode == "writer" {
        host.core_request(request).await
    } else {
        host.forward(request, &c.peer(&c.writer_id).unwrap().base_url, None, true)
            .await
    };
    let response = result
        .unwrap_or_else(|_| unavailable("当前写入节点暂不可达；为避免双写，不会自动晋升副本"));
    let (parts, body) = response.into_parts();
    let stream = Box::pin(body.into_data_stream());
    let guarded = futures::stream::try_unfold((stream, guard), |(mut stream, guard)| async move {
        Ok::<_, axum::Error>(
            stream
                .try_next()
                .await?
                .map(|bytes| (bytes, (stream, guard))),
        )
    });
    Response::from_parts(parts, Body::from_stream(guarded))
}

pub async fn run(file: &Path) -> Result<()> {
    let file = std::fs::canonicalize(file)?;
    let config: SiteConfig = serde_json::from_slice(&std::fs::read(&file)?)?;
    config.validate()?;
    private_dir(&config.data_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.data_dir.join(".site.lock"))?;
    anyhow::ensure!(
        fs4::fs_std::FileExt::try_lock_exclusive(&lock)?,
        "站点目录已由其他进程使用"
    );
    let bind = config.bind;
    let load_snapshot = |name: &str| {
        std::fs::read(config.data_dir.join(name))
            .ok()
            .and_then(|v| serde_json::from_slice::<Snapshot>(&v).ok())
            .filter(|s| s.cluster_id == config.cluster_id)
    };
    let snapshot = load_snapshot("latest.json");
    let replica = load_snapshot("replica.json");
    let host = Arc::new(Host {
        file,
        config: RwLock::new(config),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(600))
            .build()?,
        children: Mutex::new(Children {
            core: None,
            core_url: None,
            edge: None,
            edge_url: None,
            core_key: String::new(),
        }),
        gate: Arc::new(RwLock::new(())),
        operation: Mutex::new(()),
        snapshot: RwLock::new(snapshot),
        replica: RwLock::new(replica),
        error: RwLock::new(None),
    });
    if host.config.read().await.mode == "writer" {
        host.start_core().await?;
    } else {
        host.start_edge().await?;
    }
    let heart = host.clone();
    tokio::spawn(async move {
        loop {
            // Hold the config read lock through lease persistence so an old
            // heartbeat can never overwrite a durable fence.
            let c = heart.config.read().await;
            if c.mode == "writer" {
                let key = heart.children.lock().await.core_key.clone();
                if let Err(e) = write_lease(&c, &c.data_dir.join("runtime/lease.json"), &key) {
                    *heart.error.write().await = Some(e.to_string());
                }
            }
            drop(c);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    let worker = host.clone();
    tokio::spawn(async move {
        loop {
            let interval = worker.config.read().await.sync_interval_secs;
            if let Ok(_operation) = worker.operation.try_lock() {
                let c = worker.config.read().await.clone();
                let result: Result<()> = async {
                    if c.mode == "fenced" {
                        return worker.resume_transfer().await;
                    }
                    if c.mode == "writer" {
                        if !worker.core_ready().await {
                            worker.start_core().await?;
                        }
                        worker.make_checkpoint(false).await?;
                        return Ok(());
                    }
                    worker.follow_writer().await?;
                    let c = worker.config.read().await.clone();
                    let v = worker.peer_json(&c, &c.writer_id, "_site/latest").await?;
                    if let Some(id) = v["snapshotId"].as_str() {
                        if worker.replica.read().await.as_ref().map(|r| r.id.as_str()) != Some(id) {
                            worker.sync_from(&c.writer_id, id, false).await?;
                        }
                    }
                    Ok(())
                }
                .await;
                *worker.error.write().await = result.err().map(|e| e.to_string());
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
    let app = Router::new()
        .route("/site-health", get(health))
        .route("/_site/state", get(host_state))
        .route("/_site/latest", get(latest))
        .route("/_site/snapshots/{id}", get(snapshot_get))
        .route("/_site/objects/{hash}", get(object_get))
        .route("/_site/activate", post(activate))
        .route("/_site/shutdown", post(shutdown_host))
        .route("/api/sites", get(sites))
        .route("/admin/api/sites", get(sites))
        .route("/api/sites/sync", post(sync_now))
        .route("/admin/api/sites/sync", post(sync_now))
        .route("/api/sites/settings", post(local_settings))
        .route("/admin/api/sites/settings", post(local_settings))
        .route("/api/sites/transfer", post(transfer))
        .route("/admin/api/sites/transfer", post(transfer))
        .fallback(proxy)
        .layer(axum::extract::DefaultBodyLimit::max(16384))
        .with_state(host.clone());
    tracing::info!("complete site listening on {bind}");
    axum::serve(tokio::net::TcpListener::bind(bind).await?, app)
        .with_graceful_shutdown(shutdown_child())
        .await?;
    drop(lock);
    Ok(())
}
