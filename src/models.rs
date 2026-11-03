use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CatalogFile {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(rename = "formatVersion", default = "default_format_version")]
    pub format_version: u32,
    #[serde(rename = "minFormatVersion", default = "default_min_format_version")]
    pub min_format_version: u32,
    #[serde(rename = "supportedFormats", default)]
    pub supported_formats: Vec<u32>,
    #[serde(rename = "generatedAt", default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub themes: Vec<CatalogEntry>,
}

fn default_schema() -> u32 {
    1
}
fn default_format_version() -> u32 {
    2
}
fn default_min_format_version() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct CatalogEntry {
    pub id: String,
    #[serde(default)]
    pub version: u32,
    #[serde(rename = "formatVersion", default)]
    pub format_version: u32,
    #[serde(rename = "minManagerVersion", default)]
    pub min_manager_version: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub path: String,
    #[serde(rename = "previewPath", default)]
    pub preview_path: String,
    #[serde(
        rename = "avatarPath",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub avatar_path: Option<String>,
    #[serde(rename = "screenshotPaths", default)]
    pub screenshot_paths: Vec<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    #[serde(default)]
    pub changelog: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Meta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub license: String,
    #[serde(rename = "minManagerVersion", default)]
    pub min_manager_version: i64,
    #[serde(rename = "formatVersion", default)]
    pub format_version: u32,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub screenshots: Vec<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    #[serde(default)]
    pub changelog: String,
    #[serde(rename = "withdrawnAt", default)]
    pub withdrawn_at: Option<String>,
    #[serde(rename = "assetSize", default)]
    pub asset_size: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(rename = "authorHash", default)]
    pub author_hash: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Meta {
    pub fn asset_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for value in [&self.package, &self.preview] {
            if !value.is_empty() {
                keys.push(value.clone());
            }
        }
        if let Some(avatar) = &self.avatar {
            if !avatar.is_empty() {
                keys.push(avatar.clone());
            }
        }
        keys.extend(self.screenshots.iter().cloned());
        keys
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PendingMeta {
    #[serde(rename = "submissionId", default)]
    pub submission_id: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub license: String,
    #[serde(rename = "minManagerVersion", default)]
    pub min_manager_version: i64,
    #[serde(default)]
    pub changelog: String,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub screenshots: Vec<String>,
    #[serde(rename = "submittedAt", default)]
    pub submitted_at: String,
    #[serde(default)]
    pub remote: String,
    #[serde(rename = "authorHash", default)]
    pub author_hash: String,
    #[serde(rename = "authorFingerprint", default)]
    pub author_fingerprint: String,
    #[serde(rename = "submitterUserId", default)]
    pub submitter_user_id: String,
    #[serde(rename = "formatVersion", default)]
    pub format_version: u32,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub validation: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AuthorsFile {
    #[serde(default = "default_authors_version")]
    pub version: u32,
    #[serde(default)]
    pub authors: BTreeMap<String, AuthorRecord>,
}

fn default_authors_version() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AuthorRecord {
    #[serde(default)]
    pub fingerprint: String,
    #[serde(rename = "approvedAt", default)]
    pub approved_at: String,
    #[serde(rename = "lastUsedAt", default)]
    pub last_used_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct JobState {
    pub id: String,
    pub kind: String,
    pub running: bool,
    pub done: bool,
    pub error: Option<String>,
    pub message: String,
    pub progress: u64,
    pub total: u64,
    #[serde(rename = "startedAt")]
    pub started_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageUsage {
    pub limit: u64,
    pub used: u64,
    pub free: u64,
    #[serde(rename = "diskFree")]
    pub disk_free: u64,
    pub assets: u64,
    pub pending: u64,
    pub backend: String,
}

#[derive(Clone, Debug)]
pub struct FilePart {
    pub filename: String,
    pub ext: String,
    pub data: Vec<u8>,
}

impl FilePart {
    pub fn new(filename: String, data: Vec<u8>) -> Self {
        let ext = crate::util::file_extension(&filename);
        Self {
            filename,
            ext,
            data,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PackageInfo {
    pub deep: bool,
    pub package_version: u32,
    pub has_background: bool,
    pub has_card_background: bool,
    pub sticker_count: usize,
}

impl PackageInfo {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "deep": self.deep,
            "packageVersion": self.package_version,
            "hasBackground": self.has_background,
            "hasCardBackground": self.has_card_background,
            "stickerCount": self.sticker_count,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UploadForm {
    pub id: String,
    pub version: String,
    pub format_version: Option<String>,
    pub name: String,
    pub author: String,
    pub author_id: Option<String>,
    pub owner_hash: Option<String>,
    pub description: String,
    pub tags: String,
    pub license: String,
    pub min_manager_version: String,
    pub changelog: String,
    pub upload_id: Option<String>,
    pub package: FilePart,
    pub preview: FilePart,
    pub avatar: Option<FilePart>,
    pub screenshots: Vec<FilePart>,
}

impl UploadForm {
    pub fn format_version_value(&self) -> Option<u32> {
        self.format_version
            .as_deref()
            .and_then(|value| value.trim().parse::<u32>().ok())
    }
}
