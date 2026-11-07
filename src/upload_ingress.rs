//! Scoped upload capabilities and bounded, private staging on an edge.
//! Staged bytes are never published by the edge; the primary owns validation and commit.
use std::path::{Path, PathBuf};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore};

pub const TICKET_HEADER: &str = "X-Sukisu-Upload-Ticket";
pub const GRANT_SECONDS: i64 = 30 * 60;
pub const RETENTION_SECONDS: i64 = 24 * 60 * 60;
pub const MAX_CHUNK: u64 = 8 * 1024 * 1024;
pub fn default_quota() -> u64 {
    1024 * 1024 * 1024
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    pub id: String,
    pub size: u64,
    pub chunk_size: u64,
    pub expires: i64,
}

impl Grant {
    pub fn valid(&self, now: i64) -> bool {
        valid_id(&self.id)
            && self.size > 0
            && self.size <= crate::config::MAX_PACKAGE_BYTES as u64
            && (1024 * 1024..=MAX_CHUNK).contains(&self.chunk_size)
            && self.expires > now
            && self.expires <= now + GRANT_SECONDS
    }
    pub fn expected_len(&self, index: u64) -> Option<u64> {
        if self.chunk_size == 0 {
            return None;
        }
        (index < self.size.div_ceil(self.chunk_size))
            .then(|| (self.size - index * self.chunk_size).min(self.chunk_size))
    }
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

pub fn seal(secret: &str, grant: &Grant) -> Result<String> {
    let key = hex::decode(secret)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("无效节点密钥"))?;
    let mut nonce = [0; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let mut payload = b"sukisu-upload-v1\n".to_vec();
    payload.extend(serde_json::to_vec(grant)?);
    let bytes = cipher
        .encrypt(Nonce::from_slice(&nonce), payload.as_ref())
        .map_err(|_| anyhow::anyhow!("上传凭证生成失败"))?;
    Ok(format!("{}.{}", hex::encode(nonce), hex::encode(bytes)))
}

pub fn open(secret: &str, token: &str, now: i64) -> Option<Grant> {
    if token.len() > 2048 {
        return None;
    }
    let (nonce, bytes) = token.split_once('.')?;
    let nonce = hex::decode(nonce).ok()?;
    if nonce.len() != 12 {
        return None;
    }
    let key = hex::decode(secret).ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let bytes = cipher
        .decrypt(Nonce::from_slice(&nonce), hex::decode(bytes).ok()?.as_ref())
        .ok()?;
    let bytes = bytes.strip_prefix(b"sukisu-upload-v1\n")?;
    let grant: Grant = serde_json::from_slice(bytes).ok()?;
    grant.valid(now).then_some(grant)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub id: String,
    pub index: u64,
    pub size: u64,
    pub sha256: String,
}

pub struct Inbox {
    root: PathBuf,
    quota: u64,
    /// Bytes reserved or already staged. Quota checks update this atomically so
    /// concurrent workers never wait on a directory walk.
    used: std::sync::atomic::AtomicU64,
    last_scan: Mutex<std::time::Instant>,
    pub permits: Semaphore,
}

impl Inbox {
    pub async fn new(root: PathBuf, quota: u64) -> Result<Self> {
        anyhow::ensure!(
            quota >= crate::config::MAX_PACKAGE_BYTES as u64,
            "上传暂存配额至少为 128 MiB"
        );
        tokio::fs::create_dir_all(&root).await?;
        let inbox = Self {
            root,
            quota,
            used: std::sync::atomic::AtomicU64::new(0),
            last_scan: Mutex::new(std::time::Instant::now()),
            permits: Semaphore::new(4),
        };
        let used = inbox.usage_and_cleanup().await?;
        inbox
            .used
            .store(used, std::sync::atomic::Ordering::Relaxed);
        Ok(inbox)
    }

    // Only this inbox's exact session directories are eligible for expiry; never
    // scan the cache objects or arbitrary paths. Runs on startup and at most once
    // per scan interval, not on every uploaded chunk.
    async fn usage_and_cleanup(&self) -> Result<u64> {
        let mut usage = 0u64;
        let mut dirs = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = dirs.next_entry().await? {
            let id = entry.file_name().to_string_lossy().to_string();
            if !valid_id(&id) || !entry.file_type().await?.is_dir() {
                continue;
            }
            let modified = entry.metadata().await?.modified()?;
            if modified
                .elapsed()
                .is_ok_and(|age| age > Duration::from_secs(RETENTION_SECONDS as u64))
            {
                tokio::fs::remove_dir_all(entry.path()).await?;
                continue;
            }
            let mut files = tokio::fs::read_dir(entry.path()).await?;
            while let Some(file) = files.next_entry().await? {
                if file.file_type().await?.is_file() {
                    usage = usage
                        .checked_add(file.metadata().await?.len())
                        .context("暂存大小溢出")?;
                }
            }
        }
        Ok(usage)
    }

    pub async fn usage(&self) -> Result<u64> {
        let mut last_scan = self.last_scan.lock().await;
        if last_scan.elapsed() >= Duration::from_secs(30) {
            let used = self.usage_and_cleanup().await?;
            self.used
                .store(used, std::sync::atomic::Ordering::Relaxed);
            *last_scan = std::time::Instant::now();
        }
        Ok(self.used.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn reserve(&self, bytes: u64) -> bool {
        use std::sync::atomic::Ordering;

        let reservation = bytes.saturating_add(4096);
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(reservation) else {
                return false;
            };
            if next > self.quota {
                return false;
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    pub async fn put(&self, grant: &Grant, index: u64, bytes: &[u8]) -> Result<Receipt> {
        anyhow::ensure!(
            grant.valid(chrono::Utc::now().timestamp()),
            "上传凭证无效或已过期"
        );
        let expected = grant.expected_len(index).context("分片序号超出范围")?;
        anyhow::ensure!(bytes.len() as u64 == expected, "分片长度错误");
        let hash = hex::encode(Sha256::digest(bytes));
        let dir = self.root.join(&grant.id);
        if let Ok(existing) = tokio::fs::read(dir.join(format!("{index}.part"))).await {
            anyhow::ensure!(existing == bytes, "同一分片重试的内容发生变化");
            return Ok(Receipt {
                id: grant.id.clone(),
                index,
                size: expected,
                sha256: hash,
            });
        }
        anyhow::ensure!(self.reserve(expected), "节点上传暂存配额不足");
        let outcome = async {
            let root = self.root.clone();
            let free = tokio::task::spawn_blocking(move || fs4::available_space(root)).await??;
            anyhow::ensure!(
                free > expected + crate::config::DISK_RESERVE_BYTES,
                "节点磁盘空间不足"
            );
            tokio::fs::create_dir_all(&dir).await?;
            if let Ok(raw) = tokio::fs::read(dir.join("grant.json")).await {
                let previous: Grant = serde_json::from_slice(&raw)?;
                anyhow::ensure!(
                    previous.id == grant.id
                        && previous.size == grant.size
                        && previous.chunk_size == grant.chunk_size,
                    "上传会话参数冲突"
                );
            }
            crate::config::write_atomic_async(
                &dir.join("grant.json"),
                &serde_json::to_vec(grant)?,
                0o600,
            )
            .await?;
            write_part(&dir, index, bytes).await?;
            Ok::<Receipt, anyhow::Error>(Receipt {
                id: grant.id.clone(),
                index,
                size: expected,
                sha256: hash,
            })
        }
        .await;
        if outcome.is_err() {
            self.used.fetch_sub(
                expected.saturating_add(4096),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        outcome
    }

    pub async fn read(&self, id: &str, index: u64) -> Result<Vec<u8>> {
        anyhow::ensure!(valid_id(id), "上传会话不存在");
        let dir = self.root.join(id);
        let grant: Grant = serde_json::from_slice(&tokio::fs::read(dir.join("grant.json")).await?)?;
        anyhow::ensure!(
            chrono::Utc::now().timestamp() < grant.expires + RETENTION_SECONDS,
            "上传暂存已过期"
        );
        let expected = grant.expected_len(index).context("无效分片")?;
        let bytes = tokio::fs::read(dir.join(format!("{index}.part"))).await?;
        anyhow::ensure!(bytes.len() as u64 == expected, "暂存分片损坏");
        Ok(bytes)
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        anyhow::ensure!(valid_id(id), "上传会话不存在");
        match tokio::fs::remove_dir_all(self.root.join(id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Concurrent-safe chunk write: each index is written to a private temp file and
/// renamed into place, so workers no longer need to serialize on a global mutex.
/// The session directory is never created here; a chunk that arrives after the
/// session was cancelled fails instead of resurrecting the directory.
pub async fn write_part(dir: &Path, index: u64, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let destination = dir.join(format!("{index}.part"));
    match tokio::fs::metadata(&destination).await {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.len() == bytes.len() as u64,
                "同一分片重试的大小发生变化"
            );
            anyhow::ensure!(
                tokio::fs::read(&destination).await? == bytes,
                "同一分片重试的内容发生变化"
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temp = dir.join(format!(
        ".{index}.part.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    tokio::fs::write(&temp, bytes)
        .await
        .map_err(|error| anyhow::anyhow!("写入上传分片失败：{error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).await
        {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error.into());
        }
    }
    if let Err(error) = tokio::fs::rename(&temp, &destination).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }
    if let Ok(file) = tokio::fs::OpenOptions::new().write(true).open(&destination).await {
        file.sync_all().await?;
    }
    Ok(())
}
