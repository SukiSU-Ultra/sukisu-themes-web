use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::Object;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::AsyncSeekExt;
use tokio::sync::RwLock;
use walkdir::WalkDir;

use crate::config::{
    write_atomic_async, Backend, Paths, StorageSettings, ASSET_DIRS, DISK_RESERVE_BYTES,
};
use crate::jobs::JobProgress;
use crate::util::{content_type_for, normalize_asset_key, parse_byte_range, sha256_hex, ByteRange};

#[derive(thiserror::Error, Debug)]
pub enum ServeError {
    #[error("not found")]
    NotFound,
    #[error("range not satisfiable")]
    RangeNotSatisfiable(u64),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub enum ServeBody {
    Local(tokio::fs::File),
    S3(ByteStream),
    Remote(reqwest::Response),
}

pub struct ServeObject {
    pub body: ServeBody,
    pub content_type: String,
    pub content_length: u64,
    pub content_range: Option<String>,
    pub status: u16,
}

#[derive(Clone)]
pub struct LocalStore {
    pub root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn resolve(&self, key: &str) -> Option<PathBuf> {
        let key = normalize_asset_key(key)?;
        let target = self.root.join(key);
        if target.starts_with(&self.root) {
            Some(target)
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct S3Store {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
    pub prefix: String,
}

#[derive(Clone)]
pub struct AlistStore {
    client: reqwest::Client,
    base: String,
    token: String,
    root: String,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct AlistResponse<T> {
    code: i64,
    message: Option<String>,
    data: Option<T>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct AlistListData {
    // AList returns `"content": null` for empty directories.
    #[serde(default)]
    content: Option<Vec<AlistEntry>>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct AlistEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    is_dir: bool,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct AlistFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    is_dir: bool,
    #[serde(default)]
    raw_url: Option<String>,
}

impl AlistStore {
    pub fn new(client: reqwest::Client, base: &str, token: &str, root: &str) -> Result<Self> {
        let base = base.trim().trim_end_matches('/').to_string();
        let parsed = reqwest::Url::parse(&base).context("AList 地址无效")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("AList 地址必须是 http(s)");
        }
        let mut root = root.trim().to_string();
        if root.is_empty() {
            root = "/".to_string();
        }
        if !root.starts_with('/') || root.split('/').any(|part| part == "..") {
            bail!("AList 根目录无效");
        }
        if root != "/" {
            root = root.trim_end_matches('/').to_string();
        }
        Ok(Self {
            client,
            base,
            token: token.trim().to_string(),
            root,
        })
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("{}/{}", self.base, endpoint.trim_start_matches('/'))
    }

    fn full_path(&self, key: &str) -> Result<String> {
        let key = normalize_asset_key(key).ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
        Ok(if self.root == "/" {
            format!("/{key}")
        } else {
            format!("{}/{}", self.root, key)
        })
    }

    async fn post<T: DeserializeOwned>(&self, endpoint: &str, body: Value) -> Result<T> {
        let url = self.api_url(endpoint);
        let mut request = self
            .client
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(20));
        if !self.token.is_empty() {
            request = request.header("Authorization", &self.token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("无法连接 AList：{url}"))?;
        let status = response.status();
        let payload: AlistResponse<T> = response
            .json()
            .await
            .with_context(|| format!("AList 返回格式错误（HTTP {status}）"))?;
        if !status.is_success() || payload.code != 200 {
            let message = payload
                .message
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("AList 请求失败（HTTP {status}）"));
            bail!("{message}");
        }
        payload
            .data
            .ok_or_else(|| anyhow::anyhow!("AList 返回数据为空"))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let _: Value = self.post("api/fs/mkdir", json!({ "path": path })).await?;
        Ok(())
    }

    async fn ensure_parent(&self, full: &str) {
        let trimmed = full.trim_start_matches('/');
        let parts = trimmed.split('/').collect::<Vec<_>>();
        let mut current = String::new();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            current.push('/');
            current.push_str(part);
            let _ = self.mkdir(&current).await;
        }
    }

    async fn get_file(&self, key: &str) -> Result<AlistFile> {
        let path = self.full_path(key)?;
        self.post("api/fs/get", json!({ "path": path, "password": "" }))
            .await
    }

    async fn fetch_raw(&self, raw_url: &str, range: Option<&str>) -> Result<reqwest::Response> {
        let url = resolve_url(&self.base, raw_url)?;
        let mut request = self
            .client
            .get(url.clone())
            .timeout(Duration::from_secs(600));
        if same_origin(&url, &self.base) && !self.token.is_empty() {
            request = request.header("Authorization", &self.token);
        }
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        request.send().await.context("对象存储读取失败")
    }

    pub async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<()> {
        let full = self.full_path(key)?;
        self.ensure_parent(&full).await;
        let encoded = utf8_percent_encode(&full, NON_ALPHANUMERIC).to_string();
        let mut request = self
            .client
            .put(self.api_url("api/fs/put"))
            .header("File-Path", encoded)
            .header("Content-Type", content_type)
            .timeout(Duration::from_secs(600));
        if !self.token.is_empty() {
            request = request.header("Authorization", &self.token);
        }
        let response = request
            .body(data.to_vec())
            .send()
            .await
            .context("对象存储写入失败")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = payload.get("code").and_then(Value::as_i64);
        if !status.is_success() || !(code == Some(200) || text.trim().is_empty()) {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("对象存储写入失败");
            bail!("{message}");
        }
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let full = self.full_path(key)?;
        let parent = full
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("/");
        let name = full.rsplit('/').next().unwrap_or_default();
        let _: Value = self
            .post("api/fs/remove", json!({ "dir": parent, "names": [name] }))
            .await
            .unwrap_or(Value::Null);
        Ok(())
    }

    pub async fn size(&self, key: &str) -> u64 {
        self.get_file(key)
            .await
            .ok()
            .and_then(|file| file.size.checked_abs())
            .map(|value| value as u64)
            .unwrap_or(0)
    }

    pub async fn read(&self, key: &str) -> Result<Vec<u8>> {
        let file = self.get_file(key).await?;
        let raw_url = file
            .raw_url
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("AList 没有返回下载地址"))?;
        let response = self.fetch_raw(&raw_url, None).await?;
        if !response.status().is_success() {
            bail!("对象存储读取失败：HTTP {}", response.status());
        }
        let bytes = response.bytes().await.context("读取对象存储响应失败")?;
        Ok(bytes.to_vec())
    }

    pub async fn serve(&self, key: &str, range: Option<&str>) -> Result<ServeObject, ServeError> {
        let file = self.get_file(key).await.map_err(|_| ServeError::NotFound)?;
        if file.is_dir {
            return Err(ServeError::NotFound);
        }
        let size = file.size.max(0) as u64;
        let (start, end) = match parse_byte_range(range, size) {
            ByteRange::Full => (0, size.saturating_sub(1)),
            ByteRange::Partial { start, end } => (start, end),
            ByteRange::Invalid => return Err(ServeError::RangeNotSatisfiable(size)),
        };
        let length = end.saturating_sub(start) + 1;
        let range_header = if range.is_some() {
            Some(format!("bytes={start}-{end}"))
        } else {
            None
        };
        let raw_url = file
            .raw_url
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ServeError::Other(anyhow::anyhow!("AList 没有返回下载地址")))?;
        let response = self
            .fetch_raw(&raw_url, range_header.as_deref())
            .await
            .map_err(ServeError::Other)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ServeError::NotFound);
        }
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(ServeError::RangeNotSatisfiable(size));
        }
        if !status.is_success() {
            return Err(ServeError::Other(anyhow::anyhow!(
                "对象存储读取失败：HTTP {status}"
            )));
        }
        let content_length = response.content_length().unwrap_or(length);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| content_type_for(key));
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        Ok(ServeObject {
            body: ServeBody::Remote(response),
            content_type,
            content_length,
            content_range,
            status: status.as_u16(),
        })
    }

    pub async fn list(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for directory in ASSET_DIRS {
            let path = if self.root == "/" {
                format!("/{directory}")
            } else {
                format!("{}/{directory}", self.root)
            };
            self.walk_dir(&path, &mut keys, 0).await?;
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    async fn walk_dir(&self, path: &str, keys: &mut Vec<String>, depth: usize) -> Result<()> {
        if depth > 8 || keys.len() > 10_000 {
            return Ok(());
        }
        let data: AlistListData = self
            .post(
                "api/fs/list",
                json!({
                    "path": path,
                    "password": "",
                    "page": 1,
                    "per_page": 1000,
                    "refresh": false
                }),
            )
            .await?;
        for entry in data.content.unwrap_or_default() {
            let full = if path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", path.trim_end_matches('/'), entry.name)
            };
            if entry.is_dir {
                Box::pin(self.walk_dir(&full, keys, depth + 1)).await?;
            } else {
                let relative = if self.root == "/" {
                    full.trim_start_matches('/')
                } else {
                    full.strip_prefix(&self.root)
                        .unwrap_or(&full)
                        .trim_start_matches('/')
                };
                if normalize_asset_key(relative).is_some() {
                    keys.push(relative.to_string());
                }
            }
        }
        Ok(())
    }
}

fn resolve_url(base: &str, raw: &str) -> Result<reqwest::Url> {
    let raw = raw.trim();
    if let Ok(url) = reqwest::Url::parse(raw) {
        return Ok(url);
    }
    let base = reqwest::Url::parse(base).context("AList 地址无效")?;
    base.join(raw).context("AList 下载地址无效")
}

fn same_origin(left: &reqwest::Url, right: &str) -> bool {
    let Ok(right) = reqwest::Url::parse(right) else {
        return false;
    };
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

#[derive(Clone)]
pub struct LocationSpec {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub writable: bool,
    pub quota_bytes: u64,
    pub priority: i64,
    pub local_path: String,
    pub alist_url: String,
    pub alist_token: String,
    pub alist_root: String,
}

#[derive(Clone)]
pub struct LocationStore {
    pub id: String,
    pub name: String,
    pub store: AssetStore,
    pub quota_bytes: u64,
    pub priority: i64,
    pub enabled: bool,
    pub local_root: Option<PathBuf>,
}

#[derive(Clone)]
pub struct MultiStore {
    primary: Arc<AssetStore>,
    locations: Arc<RwLock<Vec<LocationStore>>>,
    http: reqwest::Client,
}

impl MultiStore {
    async fn store_for_key(&self, key: &str) -> AssetStore {
        if let Some(location_id) = key_location_id(key) {
            if let Some(location) = self
                .locations
                .read()
                .await
                .iter()
                .find(|location| location.id == location_id)
            {
                return location.store.clone();
            }
        }
        self.primary.as_ref().clone()
    }

    async fn set_locations(&self, specs: Vec<LocationSpec>) {
        let locations = build_locations(specs, &self.http);
        *self.locations.write().await = locations;
    }
}

fn key_location_id(key: &str) -> Option<&str> {
    let mut parts = key.split('/');
    let directory = parts.next()?;
    if !ASSET_DIRS.contains(&directory) {
        return None;
    }
    if parts.next()? != "_loc" {
        return None;
    }
    parts.next()
}

fn build_locations(specs: Vec<LocationSpec>, http: &reqwest::Client) -> Vec<LocationStore> {
    let mut locations = specs
        .into_iter()
        .filter(|spec| spec.writable)
        .filter_map(|spec| {
            let (store, local_root) = match spec.kind.as_str() {
                "local" => {
                    let path = PathBuf::from(spec.local_path.trim());
                    if path.as_os_str().is_empty() {
                        return None;
                    }
                    (AssetStore::Local(LocalStore::new(path.clone())), Some(path))
                }
                "alist" => match AlistStore::new(
                    http.clone(),
                    &spec.alist_url,
                    &spec.alist_token,
                    &spec.alist_root,
                ) {
                    Ok(store) => (AssetStore::Alist(store), None),
                    Err(error) => {
                        tracing::warn!("AList location {} skipped: {error}", spec.name);
                        return None;
                    }
                },
                _ => return None,
            };
            Some(LocationStore {
                id: spec.id,
                name: spec.name,
                store,
                quota_bytes: spec.quota_bytes,
                priority: spec.priority,
                enabled: spec.enabled,
                local_root,
            })
        })
        .collect::<Vec<_>>();
    locations.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.name.cmp(&right.name))
    });
    locations
}

pub fn build_multi(
    primary: AssetStore,
    specs: Vec<LocationSpec>,
    http: reqwest::Client,
) -> AssetStore {
    let locations = build_locations(specs, &http);
    AssetStore::Multi(MultiStore {
        primary: Arc::new(primary),
        locations: Arc::new(RwLock::new(locations)),
        http,
    })
}

#[derive(Clone)]
pub enum AssetStore {
    Local(LocalStore),
    S3(S3Store),
    Alist(AlistStore),
    Multi(MultiStore),
}

impl AssetStore {
    pub fn backend(&self) -> &'static str {
        match self {
            AssetStore::Local(_) => "local",
            AssetStore::S3(_) => "s3",
            AssetStore::Alist(_) => "alist",
            AssetStore::Multi(multi) => multi.primary.backend(),
        }
    }

    pub fn local_root(&self) -> Option<&Path> {
        match self {
            AssetStore::Local(store) => Some(&store.root),
            AssetStore::S3(_) | AssetStore::Alist(_) => None,
            AssetStore::Multi(multi) => multi.primary.local_root(),
        }
    }

    pub fn primary(&self) -> &AssetStore {
        match self {
            AssetStore::Multi(multi) => multi.primary.as_ref(),
            _ => self,
        }
    }

    pub async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<()> {
        match self {
            AssetStore::Local(store) => {
                let path = store
                    .resolve(key)
                    .ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
                write_atomic_async(&path, data, 0o644)
                    .await
                    .context("写入本地资源失败")?;
                Ok(())
            }
            AssetStore::S3(store) => {
                let key =
                    normalize_asset_key(key).ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
                store
                    .client
                    .put_object()
                    .bucket(&store.bucket)
                    .key(format!("{}{}", store.prefix, key))
                    .body(ByteStream::from(data.to_vec()))
                    .content_type(content_type)
                    .send()
                    .await
                    .context("对象存储写入失败")?;
                Ok(())
            }
            AssetStore::Alist(store) => store.put(key, data, content_type).await,
            AssetStore::Multi(multi) => Box::pin(multi.primary.put(key, data, content_type)).await,
        }
    }

    pub async fn put_at(
        &self,
        location: Option<&str>,
        key: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<()> {
        match self {
            AssetStore::Multi(multi) => match location {
                Some(id) => {
                    let store = multi
                        .locations
                        .read()
                        .await
                        .iter()
                        .find(|location| location.id == id)
                        .map(|location| location.store.clone());
                    match store {
                        Some(store) => Box::pin(store.put(key, data, content_type)).await,
                        None => bail!("存储位置不存在"),
                    }
                }
                None => Box::pin(multi.primary.put(key, data, content_type)).await,
            },
            _ => {
                if location.is_some() {
                    bail!("存储位置不存在");
                }
                self.put(key, data, content_type).await
            }
        }
    }

    pub async fn reload_locations(&self, specs: Vec<LocationSpec>) {
        if let AssetStore::Multi(multi) = self {
            multi.set_locations(specs).await;
        }
    }

    pub async fn choose_location(
        &self,
        new_size: u64,
        paths: &Paths,
        primary_limit: u64,
    ) -> Result<Option<String>> {
        let primary = self.primary();
        let usage =
            crate::catalog::storage_usage(primary, paths, primary.backend(), primary_limit).await;
        let disk_ok = primary.backend() != "local"
            || usage.disk_free >= new_size.saturating_add(DISK_RESERVE_BYTES);
        if usage.used.saturating_add(new_size) <= primary_limit && disk_ok {
            return Ok(None);
        }
        if let AssetStore::Multi(multi) = self {
            let locations = multi.locations.read().await.clone();
            for location in locations {
                if !location.enabled {
                    continue;
                }
                let used = crate::catalog::location_usage(paths, &location.id).await;
                if location.quota_bytes > 0 && used.saturating_add(new_size) > location.quota_bytes
                {
                    continue;
                }
                if let Some(root) = &location.local_root {
                    match fs4::available_space(root) {
                        Ok(free) if free >= new_size.saturating_add(DISK_RESERVE_BYTES) => {}
                        _ => continue,
                    }
                }
                return Ok(Some(location.id));
            }
        }
        bail!("存储配额不足，请清理空间或调整存储位置配额")
    }

    pub async fn location_usage(&self, paths: &Paths, id: &str) -> u64 {
        crate::catalog::location_usage(paths, id).await
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match self {
            AssetStore::Local(store) => {
                let Some(path) = store.resolve(key) else {
                    return Ok(());
                };
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error).context("删除本地资源失败"),
                }
            }
            AssetStore::S3(store) => {
                let key =
                    normalize_asset_key(key).ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
                store
                    .client
                    .delete_object()
                    .bucket(&store.bucket)
                    .key(format!("{}{}", store.prefix, key))
                    .send()
                    .await
                    .context("对象存储删除失败")?;
                Ok(())
            }
            AssetStore::Alist(store) => store.delete(key).await,
            AssetStore::Multi(multi) => {
                let store = multi.store_for_key(key).await;
                Box::pin(store.delete(key)).await
            }
        }
    }

    pub async fn size(&self, key: &str) -> u64 {
        match self {
            AssetStore::Local(store) => {
                let Some(path) = store.resolve(key) else {
                    return 0;
                };
                tokio::fs::metadata(path)
                    .await
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
            }
            AssetStore::S3(store) => {
                let Some(key) = normalize_asset_key(key) else {
                    return 0;
                };
                store
                    .client
                    .head_object()
                    .bucket(&store.bucket)
                    .key(format!("{}{}", store.prefix, key))
                    .send()
                    .await
                    .ok()
                    .and_then(|response| response.content_length())
                    .map(|value| value.max(0) as u64)
                    .unwrap_or(0)
            }
            AssetStore::Alist(store) => store.size(key).await,
            AssetStore::Multi(multi) => {
                let store = multi.store_for_key(key).await;
                Box::pin(store.size(key)).await
            }
        }
    }

    pub async fn exists(&self, key: &str) -> bool {
        self.size(key).await > 0
    }

    pub async fn read(&self, key: &str) -> Result<Vec<u8>> {
        match self {
            AssetStore::Local(store) => {
                let path = store
                    .resolve(key)
                    .ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
                tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("读取本地资源失败: {}", path.display()))
            }
            AssetStore::S3(store) => {
                let key =
                    normalize_asset_key(key).ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
                let response = store
                    .client
                    .get_object()
                    .bucket(&store.bucket)
                    .key(format!("{}{}", store.prefix, key))
                    .send()
                    .await
                    .context("对象存储读取失败")?;
                let bytes = response
                    .body
                    .collect()
                    .await
                    .context("读取对象存储响应失败")?
                    .into_bytes();
                Ok(bytes.to_vec())
            }
            AssetStore::Alist(store) => store.read(key).await,
            AssetStore::Multi(multi) => {
                let store = multi.store_for_key(key).await;
                Box::pin(store.read(key)).await
            }
        }
    }

    pub async fn sha256(&self, key: &str) -> Result<String> {
        let data = self.read(key).await?;
        Ok(sha256_hex(&data))
    }

    pub async fn list(&self) -> Result<Vec<String>> {
        match self {
            AssetStore::Local(store) => {
                let root = store.root.clone();
                let keys = tokio::task::spawn_blocking(move || {
                    let mut keys = Vec::new();
                    for name in ASSET_DIRS {
                        let directory = root.join(name);
                        if !directory.is_dir() {
                            continue;
                        }
                        for entry in WalkDir::new(&directory).into_iter().flatten() {
                            if !entry.file_type().is_file() {
                                continue;
                            }
                            let Ok(relative) = entry.path().strip_prefix(&root) else {
                                continue;
                            };
                            let key = relative.to_string_lossy().replace('\\', "/");
                            if normalize_asset_key(&key).is_some() {
                                keys.push(key);
                            }
                        }
                    }
                    keys.sort();
                    keys.dedup();
                    keys
                })
                .await
                .context("本地资源列表任务失败")?;
                Ok(keys)
            }
            AssetStore::S3(store) => {
                let mut keys = Vec::new();
                let mut paginator = store
                    .client
                    .list_objects_v2()
                    .bucket(&store.bucket)
                    .prefix(&store.prefix)
                    .into_paginator()
                    .send();
                while let Some(page) = paginator.next().await {
                    let page = page.context("对象存储列表失败")?;
                    for object in page.contents() {
                        let Some(key) = object.key() else {
                            continue;
                        };
                        let Some(relative) = key.strip_prefix(&store.prefix) else {
                            continue;
                        };
                        if normalize_asset_key(relative).is_some() {
                            keys.push(relative.to_string());
                        }
                    }
                }
                keys.sort();
                keys.dedup();
                Ok(keys)
            }
            AssetStore::Alist(store) => store.list().await,
            AssetStore::Multi(multi) => Box::pin(multi.primary.list()).await,
        }
    }

    pub async fn serve(&self, key: &str, range: Option<&str>) -> Result<ServeObject, ServeError> {
        let key = normalize_asset_key(key).ok_or(ServeError::NotFound)?;
        match self {
            AssetStore::Local(store) => {
                let path = store.resolve(&key).ok_or(ServeError::NotFound)?;
                let metadata = tokio::fs::metadata(&path)
                    .await
                    .map_err(|_| ServeError::NotFound)?;
                if !metadata.is_file() {
                    return Err(ServeError::NotFound);
                }
                let size = metadata.len();
                let (start, end) = match parse_byte_range(range, size) {
                    ByteRange::Full => (0, size.saturating_sub(1)),
                    ByteRange::Partial { start, end } => (start, end),
                    ByteRange::Invalid => return Err(ServeError::RangeNotSatisfiable(size)),
                };
                let length = end.saturating_sub(start) + 1;
                let mut file = tokio::fs::File::open(&path)
                    .await
                    .map_err(|_| ServeError::NotFound)?;
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .context("定位本地资源失败")?;
                let status = if range.is_some() { 206 } else { 200 };
                Ok(ServeObject {
                    body: ServeBody::Local(file),
                    content_type: content_type_for(&key),
                    content_length: length,
                    content_range: if status == 206 {
                        Some(format!("bytes {start}-{end}/{size}"))
                    } else {
                        None
                    },
                    status,
                })
            }
            AssetStore::S3(store) => {
                let s3_key = format!("{}{}", store.prefix, key);
                // Full downloads do not need a separate HEAD: the GET already
                // returns Content-Length. Resumed ranges still need the total
                // size to parse suffix/range headers and answer 416 locally.
                let (size, range_value) = if range.is_some() {
                    let size = self.size(&key).await;
                    if size == 0 {
                        return Err(ServeError::NotFound);
                    }
                    let value = match parse_byte_range(range, size) {
                        ByteRange::Full => None,
                        ByteRange::Partial { start, end } => {
                            Some(format!("bytes={start}-{end}"))
                        }
                        ByteRange::Invalid => {
                            return Err(ServeError::RangeNotSatisfiable(size))
                        }
                    };
                    (size, value)
                } else {
                    (0, None)
                };
                let mut request = store.client.get_object().bucket(&store.bucket).key(&s3_key);
                if let Some(value) = &range_value {
                    request = request.range(value);
                }
                let response = request.send().await.map_err(|error| {
                    if let aws_sdk_s3::error::SdkError::ServiceError(service) = &error {
                        if service.err().is_no_such_key() {
                            return ServeError::NotFound;
                        }
                    }
                    ServeError::Other(anyhow::anyhow!(error.to_string()))
                })?;
                let content_length = response
                    .content_length()
                    .map(|value| value.max(0) as u64)
                    .unwrap_or(size);
                let content_type = response
                    .content_type()
                    .map(str::to_string)
                    .unwrap_or_else(|| content_type_for(&key));
                let content_range = response.content_range().map(str::to_string);
                Ok(ServeObject {
                    body: ServeBody::S3(response.body),
                    content_type,
                    content_length,
                    content_range,
                    status: if range_value.is_some() { 206 } else { 200 },
                })
            }
            AssetStore::Alist(store) => store.serve(&key, range).await,
            AssetStore::Multi(multi) => {
                let store = multi.store_for_key(&key).await;
                Box::pin(store.serve(&key, range)).await
            }
        }
    }
}

pub fn build_store(settings: &StorageSettings) -> Result<AssetStore> {
    match settings.backend {
        Backend::Local => Ok(AssetStore::Local(LocalStore::new(settings.root.clone()))),
        Backend::S3 => {
            crate::config::validate_s3_settings(settings)?;
            let credentials = aws_sdk_s3::config::Credentials::new(
                &settings.s3_access_key,
                &settings.s3_secret_key,
                None,
                None,
                "sukisu-theme-web",
            );
            let mut builder = aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new(settings.s3_region.clone()))
                .credentials_provider(credentials)
                .force_path_style(settings.s3_path_style);
            if !settings.s3_endpoint.trim().is_empty() {
                builder = builder.endpoint_url(settings.s3_endpoint.trim());
            }
            let client = aws_sdk_s3::Client::from_conf(builder.build());
            Ok(AssetStore::S3(S3Store {
                client,
                bucket: settings.s3_bucket.clone(),
                prefix: normalize_prefix(&settings.s3_prefix),
            }))
        }
    }
}

fn normalize_prefix(value: &str) -> String {
    let mut prefix = value.trim().trim_start_matches('/').to_string();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    prefix
}

pub async fn test_s3_connection(settings: &StorageSettings) -> Result<()> {
    crate::config::validate_s3_settings(settings)?;
    let store = build_store(settings)?;
    let key = format!("avatars/__healthcheck-{}.txt", crate::util::random_hex(6));
    let payload = b"sukisu-theme-web storage healthcheck";
    store.put(&key, payload, "text/plain").await?;
    let result = store.read(&key).await;
    let _ = store.delete(&key).await;
    let data = result?;
    if data != payload {
        bail!("对象存储回读校验失败");
    }
    Ok(())
}

pub async fn copy_assets(
    source: &AssetStore,
    target: &AssetStore,
    remove_source: bool,
    progress: &JobProgress,
) -> Result<()> {
    let keys = source.list().await?;
    let mut total = 0u64;
    for key in &keys {
        total = total.saturating_add(source.size(key).await);
    }
    progress.set_total(total);
    progress.set_message(&format!("迁移 {} 个资源文件", keys.len()));
    let mut copied = 0u64;
    for key in &keys {
        let data = source.read(key).await?;
        target.put(key, &data, &content_type_for(key)).await?;
        if target.size(key).await != data.len() as u64 {
            bail!("迁移校验失败: {key}");
        }
        copied = copied.saturating_add(data.len() as u64);
        progress.set_progress(copied);
    }
    if remove_source {
        progress.set_message("删除源文件");
        for key in &keys {
            source.delete(key).await?;
        }
    }
    progress.set_message("资源迁移完成");
    Ok(())
}

pub async fn copy_state_tree(
    source: PathBuf,
    target: PathBuf,
    progress: JobProgress,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let files: Vec<PathBuf> = WalkDir::new(&source)
            .into_iter()
            .flatten()
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.path().to_path_buf())
            .collect();
        let total = files
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        progress.set_total(total);
        progress.set_message(&format!("复制 {} 个文件", files.len()));
        let mut copied = 0u64;
        for path in files {
            let relative = path.strip_prefix(&source)?;
            let destination = target.join(relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let already = destination
                .metadata()
                .ok()
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let source_size = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if !(destination.is_file() && already == source_size) {
                std::fs::copy(&path, &destination)?;
            }
            copied = copied.saturating_add(source_size);
            progress.set_progress(copied);
        }
        progress.set_message("目录复制完成");
        Ok(())
    })
    .await
    .context("目录复制任务失败")??;
    Ok(())
}

pub async fn delete_asset_keys(store: &AssetStore, keys: &[String]) {
    for key in keys {
        let _ = store.delete(key).await;
    }
}

#[allow(dead_code)]
fn object_size(object: &Object) -> u64 {
    object.size().unwrap_or(0).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn local_serve_and_range() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("avatars")).unwrap();
        std::fs::write(directory.path().join("avatars/a.jpg"), b"hello").unwrap();
        let store = AssetStore::Local(LocalStore::new(directory.path()));
        let object = store.serve("avatars/a.jpg", None).await.unwrap();
        assert_eq!(object.status, 200);
        assert_eq!(object.content_length, 5);
        let body = match object.body {
            ServeBody::Local(file) => file,
            _ => panic!("expected local body"),
        };
        let mut bytes = Vec::new();
        body.take(5).read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"hello");

        let object = store
            .serve("avatars/a.jpg", Some("bytes=1-3"))
            .await
            .unwrap();
        assert_eq!(object.status, 206);
        assert_eq!(object.content_length, 3);
        assert_eq!(object.content_range.as_deref(), Some("bytes 1-3/5"));
        let body = match object.body {
            ServeBody::Local(file) => file,
            _ => panic!("expected local body"),
        };
        let mut bytes = Vec::new();
        body.take(3).read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"ell");
    }

    #[tokio::test]
    async fn multi_store_overflows_and_resolves_location() {
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: primary.path().to_path_buf(),
            meta: primary.path().join("meta"),
            pending: primary.path().join("pending"),
            uploads: primary.path().join("uploads"),
            catalog: primary.path().join("catalog.json"),
            static_dir: primary.path().join("static"),
            authors_file: primary.path().join("authors.json"),
            token_env: primary.path().join("token.env"),
            admin_env: primary.path().join("admin.env"),
            admin_password_file: primary.path().join("admin-password"),
            admin_config: primary.path().join("config.env"),
            storage_env: primary.path().join("storage.env"),
            account_env: primary.path().join("account.env"),
            network_env: primary.path().join("network.env"),
            sources_file: primary.path().join("sources.json"),
            legacy_db: primary.path().join("account.db"),
            db_dir: primary.path().join("db-split"),
            db_layout: crate::config::DbLayout::Split,
            db: primary.path().join("db-split/account.db"),
            authors_db: primary.path().join("db-split/authors.db"),
            social_db: primary.path().join("db-split/social.db"),
            stats_db: primary.path().join("db-split/stats.db"),
            audit_db: primary.path().join("db-split/audit.db"),
        };
        std::fs::create_dir_all(&paths.meta).unwrap();
        std::fs::create_dir_all(&paths.pending).unwrap();
        std::fs::write(
            paths.meta.join("big.json"),
            br#"{"id":"big","assetSize":1000,"package":"themes/big.sukisu","preview":"previews/big.webp"}"#,
        )
        .unwrap();
        let store = build_multi(
            AssetStore::Local(LocalStore::new(primary.path())),
            vec![LocationSpec {
                id: "loc1".to_string(),
                name: "Extra".to_string(),
                kind: "local".to_string(),
                enabled: true,
                writable: true,
                quota_bytes: 0,
                priority: 1,
                local_path: extra.path().to_string_lossy().to_string(),
                alist_url: String::new(),
                alist_token: String::new(),
                alist_root: "/".to_string(),
            }],
            reqwest::Client::new(),
        );
        let selected = store.choose_location(10, &paths, 1000).await.unwrap();
        assert_eq!(selected.as_deref(), Some("loc1"));
        store
            .put_at(
                selected.as_deref(),
                "themes/_loc/loc1/test-v1.sukisu",
                b"data",
                "application/octet-stream",
            )
            .await
            .unwrap();
        assert!(extra
            .path()
            .join("themes/_loc/loc1/test-v1.sukisu")
            .is_file());
        let object = store
            .serve("themes/_loc/loc1/test-v1.sukisu", None)
            .await
            .unwrap();
        let ServeBody::Local(file) = object.body else {
            panic!("expected local body");
        };
        let mut bytes = Vec::new();
        file.take(4).read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"data");
    }

    #[tokio::test]
    async fn alist_store_put_read_and_range() {
        use axum::body::{Body, Bytes as AxumBytes};
        use axum::http::{header, HeaderMap, StatusCode};
        use axum::response::Response;
        use axum::routing::{get, post, put};
        use axum::{Json, Router};

        async fn put_handler(headers: HeaderMap, _body: AxumBytes) -> Json<Value> {
            assert!(headers.contains_key("file-path"));
            assert_eq!(
                headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok()),
                Some("token")
            );
            Json(json!({"code": 200}))
        }
        async fn ok() -> Json<Value> {
            Json(json!({"code": 200}))
        }
        async fn get_file() -> Json<Value> {
            Json(json!({
                "code": 200,
                "data": {"name": "a.sukisu", "size": 5, "is_dir": false, "raw_url": "/download"}
            }))
        }
        async fn download(headers: HeaderMap) -> Response {
            if headers
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok())
                == Some("bytes=1-3")
            {
                return Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_RANGE, "bytes 1-3/5")
                    .header(header::CONTENT_LENGTH, "3")
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from("ell"))
                    .unwrap();
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, "5")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from("hello"))
                .unwrap()
        }
        let app = Router::new()
            .route("/api/fs/put", put(put_handler))
            .route("/api/fs/mkdir", post(ok))
            .route("/api/fs/get", post(get_file))
            .route("/api/fs/remove", post(ok))
            .route("/download", get(download));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let store = AlistStore::new(
            reqwest::Client::new(),
            &format!("http://{address}"),
            "token",
            "/",
        )
        .unwrap();
        store
            .put("themes/a.sukisu", b"hello", "application/octet-stream")
            .await
            .unwrap();
        assert_eq!(store.size("themes/a.sukisu").await, 5);
        assert_eq!(store.read("themes/a.sukisu").await.unwrap(), b"hello");
        let object = store
            .serve("themes/a.sukisu", Some("bytes=1-3"))
            .await
            .unwrap();
        assert_eq!(object.status, 206);
        assert_eq!(object.content_length, 3);
        assert_eq!(object.content_range.as_deref(), Some("bytes 1-3/5"));
        let ServeBody::Remote(response) = object.body else {
            panic!("expected remote body");
        };
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"ell");
    }

    #[test]
    fn accepts_null_alist_content() {
        let data: AlistListData = serde_json::from_value(json!({ "content": null })).unwrap();
        assert!(data.content.unwrap_or_default().is_empty());
    }
}
