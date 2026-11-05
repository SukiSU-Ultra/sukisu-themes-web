use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::config::{IMAGE_EXTENSIONS, MAX_IMAGE_INPUT_BYTES, MAX_PACKAGE_BYTES, MAX_SCREENSHOTS};
use crate::jobs::JobProgress;
use crate::models::{FilePart, UploadForm};
use crate::uploads::{self, PublishError};
use crate::util::{
    file_extension, now_iso, random_hex, safe_join, valid_author_hash, valid_theme_id,
};

const MAX_SOURCES: usize = 32;
const MAX_BROWSE_ENTRIES: usize = 500;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcesFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub sources: Vec<StorageSource>,
}

fn default_version() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSource {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub quota_bytes: u64,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub local: LocalSource,
    #[serde(default)]
    pub alist: AlistSource,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_checked_at: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn default_enabled() -> bool {
    true
}

fn default_status() -> String {
    "unknown".to_string()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSource {
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlistSource {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub token: String,
    #[serde(default = "default_root")]
    pub root: String,
}

fn default_root() -> String {
    "/".to_string()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseResult {
    pub path: String,
    pub entries: Vec<BrowseEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    pub source_id: String,
    pub package_path: String,
    pub preview_path: String,
    #[serde(default)]
    pub avatar_path: Option<String>,
    #[serde(default)]
    pub screenshot_paths: Vec<String>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub format_version: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub author_hash: Option<String>,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub min_manager_version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub changelog: String,
}

pub struct SourcesStore {
    file: PathBuf,
    inner: RwLock<Vec<StorageSource>>,
}

impl SourcesStore {
    pub async fn load(file: PathBuf) -> Arc<Self> {
        let sources = match tokio::fs::read(&file).await {
            Ok(bytes) => match serde_json::from_slice::<SourcesFile>(&bytes) {
                Ok(data) => data.sources,
                Err(error) => {
                    tracing::warn!("sources.json parse warning: {error}");
                    Vec::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!("sources.json read warning: {error}");
                Vec::new()
            }
        };
        Arc::new(Self {
            file,
            inner: RwLock::new(sources),
        })
    }

    pub async fn list(&self) -> Vec<StorageSource> {
        self.inner.read().await.clone()
    }

    pub async fn get(&self, id: &str) -> Option<StorageSource> {
        self.inner
            .read()
            .await
            .iter()
            .find(|source| source.id == id)
            .cloned()
    }

    pub async fn save(&self, mut source: StorageSource) -> Result<StorageSource> {
        validate_source(&source)?;
        let mut guard = self.inner.write().await;
        let mut next = guard.clone();
        let now = now_iso();
        source.name = source.name.trim().to_string();
        source.kind = source.kind.trim().to_ascii_lowercase();
        if source.kind == "local" {
            source.local.path = source.local.path.trim().to_string();
            source.alist = AlistSource::default();
        } else {
            source.alist.url = source.alist.url.trim().trim_end_matches('/').to_string();
            source.alist.root = normalize_remote_path(&source.alist.root, "/")?;
            source.local = LocalSource::default();
        }
        if source.id.trim().is_empty() {
            let mut id = format!("src_{}", random_hex(4));
            while next.iter().any(|item| item.id == id) {
                id = format!("src_{}", random_hex(4));
            }
            source.id = id;
            source.created_at = now.clone();
        } else if let Some(existing) = next.iter().find(|item| item.id == source.id) {
            source.created_at = existing.created_at.clone();
        } else {
            source.created_at = now.clone();
        }
        if next
            .iter()
            .any(|item| item.id != source.id && item.name.eq_ignore_ascii_case(&source.name))
        {
            bail!("来源名称已存在");
        }
        if !next.iter().any(|item| item.id == source.id) && next.len() >= MAX_SOURCES {
            bail!("最多只能添加 {MAX_SOURCES} 个来源");
        }
        source.updated_at = now;
        if let Some(existing) = next.iter_mut().find(|item| item.id == source.id) {
            *existing = source.clone();
        } else {
            next.push(source.clone());
        }
        self.persist(&next).await?;
        *guard = next;
        Ok(source)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let mut guard = self.inner.write().await;
        let mut next = guard.clone();
        let before = next.len();
        next.retain(|source| source.id != id);
        if next.len() == before {
            return Ok(false);
        }
        self.persist(&next).await?;
        *guard = next;
        Ok(true)
    }

    pub async fn update_status(&self, id: &str, status: &str, error: Option<&str>) {
        let mut guard = self.inner.write().await;
        let mut next = guard.clone();
        let Some(source) = next.iter_mut().find(|source| source.id == id) else {
            return;
        };
        source.status = status.to_string();
        source.last_error = error.map(str::to_string);
        source.last_checked_at = Some(now_iso());
        let _ = self.persist(&next).await;
        *guard = next;
    }

    async fn persist(&self, sources: &[StorageSource]) -> Result<()> {
        let Some(parent) = self.file.parent() else {
            bail!("来源配置文件路径无效");
        };
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
        let data = SourcesFile {
            version: 1,
            sources: sources.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&data)?;
        let name = self
            .file
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("sources.json");
        let tmp = self
            .file
            .with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, &bytes)
            .await
            .with_context(|| format!("无法写入 {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
        }
        tokio::fs::rename(&tmp, &self.file)
            .await
            .with_context(|| format!("无法保存 {}", self.file.display()))?;
        Ok(())
    }
}

pub fn public_json(source: &StorageSource) -> Value {
    json!({
        "id": source.id,
        "name": source.name,
        "type": source.kind,
        "enabled": source.enabled,
        "writable": source.writable,
        "quotaBytes": source.quota_bytes,
        "priority": source.priority,
        "local": { "path": source.local.path },
        "alist": {
            "url": source.alist.url,
            "root": source.alist.root,
            "hasToken": !source.alist.token.is_empty(),
        },
        "status": source.status,
        "lastError": source.last_error,
        "lastCheckedAt": source.last_checked_at,
    })
}

pub async fn location_specs(sources: &SourcesStore) -> Vec<crate::storage::LocationSpec> {
    sources
        .list()
        .await
        .into_iter()
        .filter(|source| source.writable)
        .map(|source| crate::storage::LocationSpec {
            id: source.id,
            name: source.name,
            kind: source.kind,
            enabled: source.enabled,
            writable: source.writable,
            quota_bytes: source.quota_bytes,
            priority: source.priority,
            local_path: source.local.path,
            alist_url: source.alist.url,
            alist_token: source.alist.token,
            alist_root: source.alist.root,
        })
        .collect()
}

pub fn validate_source(source: &StorageSource) -> Result<()> {
    let name = source.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        bail!("来源名称需要 1-64 个字符");
    }
    match source.kind.trim().to_ascii_lowercase().as_str() {
        "local" => {
            let path = source.local.path.trim();
            if path.is_empty() {
                bail!("本地目录不能为空");
            }
            let root = Path::new(path);
            if !root.is_absolute() {
                bail!("本地目录必须是绝对路径");
            }
            let metadata =
                std::fs::metadata(root).with_context(|| format!("无法读取本地目录：{path}"))?;
            if !metadata.is_dir() {
                bail!("本地路径不是目录：{path}");
            }
            std::fs::read_dir(root).with_context(|| format!("本地目录不可读：{path}"))?;
        }
        "alist" => {
            validate_alist_url(&source.alist.url)?;
            normalize_remote_path(&source.alist.root, "/")?;
        }
        _ => bail!("不支持的来源类型"),
    }
    Ok(())
}

fn validate_alist_url(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("AList 地址不能为空");
    }
    let parsed = reqwest::Url::parse(value).context("AList 地址无效")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("AList 地址必须是 http(s)");
    }
    if parsed.host_str().is_none() {
        bail!("AList 地址缺少主机名");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("AList 地址不能包含查询参数或片段");
    }
    Ok(())
}

pub fn normalize_remote_path(value: &str, root: &str) -> Result<String> {
    let mut raw = value.trim();
    if raw.is_empty() {
        raw = if root.trim().is_empty() {
            "/"
        } else {
            root.trim()
        };
    }
    if !raw.starts_with('/') {
        bail!("路径必须以 / 开头");
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            bail!("路径不能包含 ..");
        }
        parts.push(part);
    }
    Ok(format!("/{}", parts.join("/")))
}

fn ensure_remote_under_root(path: &str, root: &str) -> Result<()> {
    let normalized = normalize_remote_path(path, root)?;
    let root = normalize_remote_path(root, "/")?;
    if root == "/" || normalized == root || normalized.starts_with(&format!("{root}/")) {
        Ok(())
    } else {
        bail!("路径不在来源根目录内");
    }
}

fn local_relative(value: &str) -> Result<String> {
    let trimmed = value.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if !crate::util::relative_path_is_safe(trimmed) {
        bail!("路径无效");
    }
    Ok(trimmed.to_string())
}

fn display_local_path(relative: &str) -> String {
    if relative.is_empty() {
        "/".to_string()
    } else {
        format!("/{relative}")
    }
}

fn join_remote(parent: &str, name: &str) -> String {
    if parent == "/" || parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

fn modified_string(value: std::io::Result<std::time::SystemTime>) -> Option<String> {
    value
        .ok()
        .map(|time| DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true))
}

pub async fn test_source(http: &reqwest::Client, source: &StorageSource) -> Result<String> {
    match source.kind.as_str() {
        "local" => {
            let path = Path::new(&source.local.path);
            let mut count = 0usize;
            let mut entries = tokio::fs::read_dir(path)
                .await
                .with_context(|| format!("本地目录不可读：{}", path.display()))?;
            while entries.next_entry().await?.is_some() {
                count += 1;
                if count > 1000 {
                    break;
                }
            }
            Ok(format!("本地目录可读，共 {count} 项"))
        }
        "alist" => {
            let entries = alist_list(http, &source.alist, &source.alist.root).await?;
            Ok(format!("AList 连接成功，共 {} 项", entries.len()))
        }
        _ => bail!("不支持的来源类型"),
    }
}

pub async fn browse_source(
    http: &reqwest::Client,
    source: &StorageSource,
    path: &str,
) -> Result<BrowseResult> {
    match source.kind.as_str() {
        "local" => {
            let relative = local_relative(path)?;
            let root = Path::new(&source.local.path);
            let root_canon = tokio::fs::canonicalize(root)
                .await
                .with_context(|| format!("本地目录不可读：{}", root.display()))?;
            let target = if relative.is_empty() {
                root_canon.clone()
            } else {
                safe_join(&root_canon, &relative).context("路径无效")?
            };
            let target_canon = tokio::fs::canonicalize(&target)
                .await
                .with_context(|| format!("目录不存在：{}", target.display()))?;
            if !target_canon.starts_with(&root_canon) {
                bail!("路径越界");
            }
            let metadata = tokio::fs::metadata(&target_canon).await?;
            if !metadata.is_dir() {
                bail!("不是目录");
            }
            let mut read_dir = tokio::fs::read_dir(&target_canon).await?;
            let mut entries = Vec::new();
            while let Some(entry) = read_dir.next_entry().await? {
                if entries.len() >= MAX_BROWSE_ENTRIES {
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.is_empty() {
                    continue;
                }
                let metadata = entry.metadata().await?;
                let is_dir = metadata.is_dir();
                let child_relative = if relative.is_empty() {
                    name.clone()
                } else {
                    format!("{relative}/{name}")
                };
                entries.push(BrowseEntry {
                    name,
                    path: display_local_path(&child_relative),
                    is_dir,
                    size: if is_dir { 0 } else { metadata.len() },
                    modified: modified_string(metadata.modified()),
                });
            }
            entries.sort_by(|left, right| {
                right
                    .is_dir
                    .cmp(&left.is_dir)
                    .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            });
            Ok(BrowseResult {
                path: display_local_path(&relative),
                entries,
            })
        }
        "alist" => {
            ensure_remote_under_root(path, &source.alist.root)?;
            let normalized = normalize_remote_path(path, &source.alist.root)?;
            let items = alist_list(http, &source.alist, &normalized).await?;
            let mut entries = items
                .into_iter()
                .filter(|item| !item.name.is_empty())
                .take(MAX_BROWSE_ENTRIES)
                .map(|item| BrowseEntry {
                    path: join_remote(&normalized, &item.name),
                    name: item.name,
                    is_dir: item.is_dir,
                    size: if item.is_dir {
                        0
                    } else {
                        item.size.max(0) as u64
                    },
                    modified: item.modified,
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| {
                right
                    .is_dir
                    .cmp(&left.is_dir)
                    .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            });
            Ok(BrowseResult {
                path: normalized,
                entries,
            })
        }
        _ => bail!("不支持的来源类型"),
    }
}

#[derive(Clone, Debug)]
struct PreparedFile {
    path: String,
    name: String,
    size: u64,
    raw_url: Option<String>,
}

async fn prepare_file(
    http: &reqwest::Client,
    source: &StorageSource,
    path: &str,
    max_bytes: u64,
    expect_image: bool,
) -> Result<PreparedFile> {
    match source.kind.as_str() {
        "local" => {
            let relative = local_relative(path)?;
            let root = Path::new(&source.local.path);
            let root_canon = tokio::fs::canonicalize(root)
                .await
                .with_context(|| format!("本地目录不可读：{}", root.display()))?;
            let target = safe_join(&root_canon, &relative).context("路径无效")?;
            let target_canon = tokio::fs::canonicalize(&target)
                .await
                .with_context(|| format!("文件不存在：{}", target.display()))?;
            if !target_canon.starts_with(&root_canon) {
                bail!("路径越界");
            }
            let metadata = tokio::fs::metadata(&target_canon).await?;
            if !metadata.is_file() {
                bail!("不是文件：{path}");
            }
            if metadata.len() > max_bytes {
                bail!("文件超过大小限制：{path}");
            }
            let name = target_canon
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("file")
                .to_string();
            let extension = file_extension(&name);
            if expect_image && !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
                bail!("不支持的图片格式：{name}");
            }
            Ok(PreparedFile {
                path: path.to_string(),
                name,
                size: metadata.len(),
                raw_url: None,
            })
        }
        "alist" => {
            ensure_remote_under_root(path, &source.alist.root)?;
            let normalized = normalize_remote_path(path, &source.alist.root)?;
            let file = alist_get(http, &source.alist, &normalized).await?;
            if file.is_dir {
                bail!("不是文件：{path}");
            }
            if file.size > max_bytes as i64 {
                bail!("文件超过大小限制：{path}");
            }
            let name = if file.name.is_empty() {
                Path::new(&normalized)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("file")
                    .to_string()
            } else {
                file.name
            };
            let extension = file_extension(&name);
            if expect_image && !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
                bail!("不支持的图片格式：{name}");
            }
            Ok(PreparedFile {
                path: normalized,
                name,
                size: file.size.max(0) as u64,
                raw_url: file.raw_url,
            })
        }
        _ => bail!("不支持的来源类型"),
    }
}

async fn download_file(
    http: &reqwest::Client,
    source: &StorageSource,
    file: &PreparedFile,
    max_bytes: u64,
    progress: Option<&JobProgress>,
) -> Result<FilePart> {
    let data = match source.kind.as_str() {
        "local" => {
            let relative = local_relative(&file.path)?;
            let root = Path::new(&source.local.path);
            let root_canon = tokio::fs::canonicalize(root).await?;
            let target = safe_join(&root_canon, &relative).context("路径无效")?;
            let bytes = tokio::fs::read(&target)
                .await
                .with_context(|| format!("无法读取文件：{}", file.path))?;
            if bytes.len() as u64 > max_bytes {
                bail!("文件超过大小限制：{}", file.name);
            }
            if let Some(progress) = progress {
                progress.add_progress(bytes.len() as u64);
            }
            bytes
        }
        "alist" => {
            let raw_url = file
                .raw_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("AList 没有返回下载地址：{}", file.name))?;
            let url = resolve_url(&source.alist.url, raw_url)?;
            let mut request = http.get(url.clone()).timeout(DOWNLOAD_TIMEOUT);
            if same_origin(&url, &source.alist.url) && !source.alist.token.is_empty() {
                request = request.header("Authorization", source.alist.token.clone());
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("下载失败：{}", file.name))?;
            if !response.status().is_success() {
                bail!("下载失败：{} HTTP {}", file.name, response.status());
            }
            if let Some(length) = response.content_length() {
                if length > max_bytes {
                    bail!("文件超过大小限制：{}", file.name);
                }
            }
            let mut response = response;
            let mut data = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("读取下载内容失败：{}", file.name))?
            {
                if data.len().saturating_add(chunk.len()) as u64 > max_bytes {
                    bail!("文件超过大小限制：{}", file.name);
                }
                data.extend_from_slice(&chunk);
                if let Some(progress) = progress {
                    progress.add_progress(chunk.len() as u64);
                }
            }
            data
        }
        _ => bail!("不支持的来源类型"),
    };
    Ok(FilePart::new(file.name.clone(), data))
}

pub async fn import_theme(
    state: &crate::api::AppState,
    actor_id: &str,
    source: &StorageSource,
    request: ImportRequest,
    progress: &JobProgress,
) -> Result<String, String> {
    if !source.enabled {
        return Err("该来源已禁用".to_string());
    }
    if request.package_path.trim().is_empty() {
        return Err("请选择主题包".to_string());
    }
    if request.preview_path.trim().is_empty() {
        return Err("请选择预览图".to_string());
    }
    if request.screenshot_paths.len() > MAX_SCREENSHOTS {
        return Err(format!("最多导入 {MAX_SCREENSHOTS} 张截图"));
    }

    let package_file = prepare_file(
        &state.http,
        source,
        request.package_path.trim(),
        MAX_PACKAGE_BYTES as u64,
        false,
    )
    .await
    .map_err(|error| error.to_string())?;
    if file_extension(&package_file.name) != ".sukisu" {
        return Err("主题包必须是 .sukisu 文件".to_string());
    }
    let preview_file = prepare_file(
        &state.http,
        source,
        request.preview_path.trim(),
        MAX_IMAGE_INPUT_BYTES as u64,
        true,
    )
    .await
    .map_err(|error| error.to_string())?;
    let avatar_file = match request
        .avatar_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(path) => Some(
            prepare_file(
                &state.http,
                source,
                path,
                MAX_IMAGE_INPUT_BYTES as u64,
                true,
            )
            .await
            .map_err(|error| error.to_string())?,
        ),
        None => None,
    };
    let mut screenshot_files = Vec::new();
    for path in request
        .screenshot_paths
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        screenshot_files.push(
            prepare_file(
                &state.http,
                source,
                path,
                MAX_IMAGE_INPUT_BYTES as u64,
                true,
            )
            .await
            .map_err(|error| error.to_string())?,
        );
    }

    let total = package_file.size
        + preview_file.size
        + avatar_file.as_ref().map(|file| file.size).unwrap_or(0)
        + screenshot_files.iter().map(|file| file.size).sum::<u64>();
    progress.set_total(total);
    progress.set_message("正在下载主题文件");

    let package = download_file(
        &state.http,
        source,
        &package_file,
        MAX_PACKAGE_BYTES as u64,
        Some(progress),
    )
    .await
    .map_err(|error| error.to_string())?;
    let preview = download_file(
        &state.http,
        source,
        &preview_file,
        MAX_IMAGE_INPUT_BYTES as u64,
        Some(progress),
    )
    .await
    .map_err(|error| error.to_string())?;
    let avatar = match avatar_file.as_ref() {
        Some(file) => Some(
            download_file(
                &state.http,
                source,
                file,
                MAX_IMAGE_INPUT_BYTES as u64,
                Some(progress),
            )
            .await
            .map_err(|error| error.to_string())?,
        ),
        None => None,
    };
    let mut screenshots = Vec::new();
    for file in &screenshot_files {
        screenshots.push(
            download_file(
                &state.http,
                source,
                file,
                MAX_IMAGE_INPUT_BYTES as u64,
                Some(progress),
            )
            .await
            .map_err(|error| error.to_string())?,
        );
    }
    progress.set_message("正在校验并发布主题");

    let requested_author_hash = request
        .author_hash
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();
    let author = if requested_author_hash.is_empty() {
        None
    } else {
        if !valid_author_hash(&requested_author_hash) {
            return Err("作者 hash 无效".to_string());
        }
        Some(
            crate::authors::author_by_hash(&state.db, &requested_author_hash)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "作者不存在".to_string())?,
        )
    };
    let requested_id = request.id.trim().to_string();
    let theme_id = if requested_id.is_empty() {
        let Some(author) = author.as_ref() else {
            return Err("新建主题需要选择作者".to_string());
        };
        crate::authors::allocate_unique_theme_id(&state.db, &state.paths, &author.hash)
            .await
            .map_err(|error| error.to_string())?
    } else {
        if !valid_theme_id(&requested_id) {
            return Err("主题 ID 只能使用小写字母、数字和 -".to_string());
        }
        requested_id
    };
    let old_meta = crate::catalog::find_theme_meta(&state.paths, &theme_id);
    if let (Some(author), Some((_, old))) = (author.as_ref(), old_meta.as_ref()) {
        if let Some(existing) = old.author_hash.as_deref() {
            if !existing.is_empty() && existing != author.hash.as_str() {
                return Err("该主题属于其他作者".to_string());
            }
        }
    }
    let publish_owner = author
        .as_ref()
        .map(|author| author.hash.clone())
        .or_else(|| {
            old_meta
                .as_ref()
                .and_then(|(_, old)| old.author_hash.clone())
                .filter(|value| !value.is_empty())
        });
    let mut author_name = request.author.trim().to_string();
    if let Some(author) = author.as_ref() {
        if author_name.is_empty() {
            author_name = author.display_name.clone().unwrap_or_default();
        }
    }
    if author_name.is_empty() {
        return Err("请填写作者名称".to_string());
    }

    let version = if request.version.trim().is_empty() {
        "1".to_string()
    } else {
        request.version.trim().to_string()
    };
    let min_manager_version = if request.min_manager_version.trim().is_empty() {
        "0".to_string()
    } else {
        request.min_manager_version.trim().to_string()
    };
    let author_avatar = avatar.clone();
    let form = UploadForm {
        id: theme_id.clone(),
        version,
        format_version: request.format_version.clone(),
        name: request.name.trim().to_string(),
        author: author_name,
        author_id: None,
        owner_hash: publish_owner.clone(),
        description: request.description,
        tags: request.tags,
        license: request.license,
        min_manager_version,
        changelog: request.changelog,
        upload_id: None,
        package,
        preview,
        avatar,
        screenshots,
    };
    let prepared = uploads::prepare_upload(form)
        .await
        .map_err(|error| error.to_string())?;
    let release_name = prepared.name.clone();
    let release_version = prepared.version as i64;
    let themes = uploads::publish_theme(
        &state.store,
        &state.settings,
        &state.paths,
        state.limit_bytes.load(std::sync::atomic::Ordering::Relaxed),
        prepared,
        publish_owner.clone(),
        0,
        old_meta,
    )
    .await
    .map_err(publish_error_message)?;
    state.refresh_withdrawn_assets();
    if let Some(hash) = publish_owner.as_deref() {
        if let Err(error) = crate::community::record_release(&state.db, hash, &theme_id, &release_name, release_version).await {
            tracing::warn!(?error, "failed to record imported theme update");
        }
    }
    if let (Some(author), Some(avatar)) = (author.as_ref(), author_avatar.as_ref()) {
        let _ = uploads::persist_author_avatar(&state.store, &author.hash, avatar).await;
    }
    let _ = crate::accounts::audit(
        &state.db,
        Some(actor_id),
        "storage.import",
        Some("theme"),
        Some(&theme_id),
        Some(&source.name),
    )
    .await;
    Ok(format!("{}，当前共 {} 个主题", theme_id, themes.len()))
}

fn publish_error_message(error: PublishError) -> String {
    match error {
        PublishError::Bad(message) | PublishError::Conflict(message) => message,
        PublishError::Internal(error) => format!("{error:#}"),
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

#[derive(Clone, Debug, Deserialize)]
struct AlistResponse<T> {
    code: i64,
    message: Option<String>,
    data: Option<T>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AlistListData {
    // AList returns `"content": null` for empty directories.
    #[serde(default)]
    content: Option<Vec<AlistEntry>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AlistEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    is_dir: bool,
    #[serde(default)]
    modified: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
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

async fn alist_post<T: DeserializeOwned>(
    http: &reqwest::Client,
    cfg: &AlistSource,
    endpoint: &str,
    body: Value,
) -> Result<T> {
    let url = format!(
        "{}/{}",
        cfg.url.trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    );
    let mut request = http.post(&url).json(&body);
    if !cfg.token.trim().is_empty() {
        request = request.header("Authorization", cfg.token.trim());
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

async fn alist_list(
    http: &reqwest::Client,
    cfg: &AlistSource,
    path: &str,
) -> Result<Vec<AlistEntry>> {
    let data: AlistListData = alist_post(
        http,
        cfg,
        "api/fs/list",
        json!({
            "path": path,
            "password": "",
            "page": 1,
            "per_page": MAX_BROWSE_ENTRIES,
            "refresh": false
        }),
    )
    .await?;
    Ok(data.content.unwrap_or_default())
}

async fn alist_get(http: &reqwest::Client, cfg: &AlistSource, path: &str) -> Result<AlistFile> {
    alist_post(
        http,
        cfg,
        "api/fs/get",
        json!({ "path": path, "password": "" }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_remote_paths() {
        assert_eq!(normalize_remote_path("/", "/").unwrap(), "/");
        assert_eq!(normalize_remote_path("/a/b/", "/").unwrap(), "/a/b");
        assert!(normalize_remote_path("../etc", "/").is_err());
        assert!(normalize_remote_path("a/b", "/").is_err());
        assert!(ensure_remote_under_root("/themes/a", "/themes").is_ok());
        assert!(ensure_remote_under_root("/other/a", "/themes").is_err());
    }

    #[test]
    fn validates_local_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = StorageSource {
            id: String::new(),
            name: "local".to_string(),
            kind: "local".to_string(),
            enabled: true,
            writable: false,
            quota_bytes: 0,
            priority: 0,
            local: LocalSource {
                path: dir.path().to_string_lossy().to_string(),
            },
            alist: AlistSource::default(),
            status: "unknown".to_string(),
            last_error: None,
            last_checked_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        validate_source(&source).unwrap();
    }

    #[test]
    fn masks_alist_token() {
        let source = StorageSource {
            id: "src_1".to_string(),
            name: "alist".to_string(),
            kind: "alist".to_string(),
            enabled: true,
            writable: false,
            quota_bytes: 0,
            priority: 0,
            local: LocalSource::default(),
            alist: AlistSource {
                url: "https://alist.example.com".to_string(),
                token: "secret".to_string(),
                root: "/themes".to_string(),
            },
            status: "unknown".to_string(),
            last_error: None,
            last_checked_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let value = public_json(&source);
        assert_eq!(value["alist"]["hasToken"], true);
        assert!(value["alist"].get("token").is_none());
    }

    #[tokio::test]
    async fn source_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sources.json");
        let store = SourcesStore::load(file.clone()).await;
        assert!(store.list().await.is_empty());
        let saved = store
            .save(StorageSource {
                id: String::new(),
                name: " NAS ".to_string(),
                kind: "local".to_string(),
                enabled: true,
                writable: false,
                quota_bytes: 0,
                priority: 0,
                local: LocalSource {
                    path: dir.path().to_string_lossy().to_string(),
                },
                alist: AlistSource::default(),
                status: "unknown".to_string(),
                last_error: None,
                last_checked_at: None,
                created_at: String::new(),
                updated_at: String::new(),
            })
            .await
            .unwrap();
        assert!(saved.id.starts_with("src_"));
        assert_eq!(saved.name, "NAS");
        assert!(file.is_file());
        let reloaded = SourcesStore::load(file).await;
        let items = reloaded.list().await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "NAS");
    }

    #[tokio::test]
    async fn alist_client_lists_and_downloads() {
        use axum::http::HeaderMap;
        use axum::routing::{get, post};
        use axum::{Json, Router};

        let app = Router::new()
            .route(
                "/api/fs/list",
                post(|headers: HeaderMap| async move {
                    if headers.get("authorization").and_then(|value| value.to_str().ok())
                        != Some("token")
                    {
                        return Json(json!({"code": 401, "message": "unauthorized"}));
                    }
                    Json(json!({
                        "code": 200,
                        "data": { "content": [
                            {"name": "a.sukisu", "size": 3, "is_dir": false, "modified": "2026-01-01T00:00:00Z"}
                        ]}
                    }))
                }),
            )
            .route(
                "/api/fs/get",
                post(|_headers: HeaderMap| async move {
                    Json(json!({
                        "code": 200,
                        "data": { "name": "a.sukisu", "size": 3, "is_dir": false, "raw_url": "/download" }
                    }))
                }),
            )
            .route(
                "/download",
                get(|| async { "abc" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let source = StorageSource {
            id: "src_1".to_string(),
            name: "alist".to_string(),
            kind: "alist".to_string(),
            enabled: true,
            writable: false,
            quota_bytes: 0,
            priority: 0,
            local: LocalSource::default(),
            alist: AlistSource {
                url: format!("http://{address}"),
                token: "token".to_string(),
                root: "/".to_string(),
            },
            status: "unknown".to_string(),
            last_error: None,
            last_checked_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let client = reqwest::Client::new();
        let items = alist_list(&client, &source.alist, "/").await.unwrap();
        assert_eq!(items.len(), 1);
        let file = prepare_file(&client, &source, "/a.sukisu", 10, false)
            .await
            .unwrap();
        let part = download_file(&client, &source, &file, 10, None)
            .await
            .unwrap();
        assert_eq!(part.data, b"abc");
    }

    #[test]
    fn accepts_null_alist_content() {
        let data: AlistListData = serde_json::from_value(json!({ "content": null })).unwrap();
        assert!(data.content.unwrap_or_default().is_empty());
    }
}
