use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::Object;
use tokio::io::AsyncSeekExt;
use walkdir::WalkDir;

use crate::config::{write_atomic, Backend, StorageSettings, ASSET_DIRS};
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
pub enum AssetStore {
    Local(LocalStore),
    S3(S3Store),
}

impl AssetStore {
    pub fn backend(&self) -> &'static str {
        match self {
            AssetStore::Local(_) => "local",
            AssetStore::S3(_) => "s3",
        }
    }

    pub fn local_root(&self) -> Option<&Path> {
        match self {
            AssetStore::Local(store) => Some(&store.root),
            AssetStore::S3(_) => None,
        }
    }

    pub async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<()> {
        match self {
            AssetStore::Local(store) => {
                let path = store
                    .resolve(key)
                    .ok_or_else(|| anyhow::anyhow!("无效的资源路径"))?;
                write_atomic(&path, data, 0o644).context("写入本地资源失败")?;
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
        }
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
                let size = self.size(&key).await;
                if size == 0 {
                    return Err(ServeError::NotFound);
                }
                let range_value = match parse_byte_range(range, size) {
                    ByteRange::Full => None,
                    ByteRange::Partial { start, end } => Some(format!("bytes={start}-{end}")),
                    ByteRange::Invalid => return Err(ServeError::RangeNotSatisfiable(size)),
                };
                let mut request = store.client.get_object().bucket(&store.bucket).key(&s3_key);
                if let Some(value) = &range_value {
                    request = request.range(value);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|error| ServeError::Other(anyhow::anyhow!(error.to_string())))?;
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
}
