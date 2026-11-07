use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use rand::RngCore;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::config::{ASSET_PREFIXES, IMAGE_EXTENSIONS};

pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn now_compact() -> String {
    Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    hex::encode(buffer)
}

pub fn random_token(bytes: usize) -> String {
    random_hex(bytes)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn author_key(author_id: &str) -> String {
    sha256_hex(author_id.as_bytes())
}

pub fn author_fingerprint(key: &str) -> String {
    if key.len() < 16 {
        return key.to_string();
    }
    format!("{}-{}", &key[..8], &key[key.len() - 8..])
}

pub fn submission_id() -> String {
    format!("{}-{}", now_compact(), random_hex(4))
}

fn theme_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$").unwrap())
}

fn submission_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]{8}-[0-9]{6}-[0-9a-f]{8}$").unwrap())
}

fn author_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._~-]{16,128}$").unwrap())
}

fn token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._~-]{16,128}$").unwrap())
}

fn author_hash_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-f]{64}$").unwrap())
}

pub fn valid_theme_id(value: &str) -> bool {
    theme_id_regex().is_match(value)
}

pub fn valid_submission_id(value: &str) -> bool {
    submission_id_regex().is_match(value)
}

pub fn valid_author_id(value: &str) -> bool {
    author_id_regex().is_match(value)
}

pub fn valid_token(value: &str) -> bool {
    token_regex().is_match(value)
}

pub fn valid_author_hash(value: &str) -> bool {
    author_hash_regex().is_match(value)
}

pub fn normalize_asset_key(key: &str) -> Option<String> {
    let key = key.trim().trim_start_matches('/');
    if key.is_empty() || key.contains("..") || key.contains('\\') {
        return None;
    }
    // Collapse `.` components so equivalent paths cannot bypass exact-match
    // checks such as the withdrawn-asset set.
    let mut parts = Vec::new();
    for component in Path::new(key).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let normalized = parts.join("/");
    if normalized.is_empty() {
        return None;
    }
    if !ASSET_PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    {
        return None;
    }
    if !relative_path_is_safe(&normalized) {
        return None;
    }
    Some(normalized)
}

pub fn relative_path_is_safe(value: &str) -> bool {
    if value.is_empty() || value.starts_with('/') || value.contains("..") || value.contains('\\') {
        return false;
    }
    Path::new(value)
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

pub fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    if !relative_path_is_safe(relative) {
        return None;
    }
    let target = root.join(relative);
    if target.starts_with(root) {
        Some(target)
    } else {
        None
    }
}

pub fn content_type_for(key: &str) -> String {
    let lower = key.to_ascii_lowercase();
    let content_type = if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".sukisu") {
        "application/octet-stream"
    } else if lower.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if lower.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    content_type.to_string()
}

pub fn image_extension_allowed(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext)
}

pub fn file_extension(filename: &str) -> String {
    Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        .unwrap_or_default()
}

pub fn path_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        .unwrap_or_default()
}

pub fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("file")
        .to_string()
}

pub enum ByteRange {
    Full,
    Partial { start: u64, end: u64 },
    Invalid,
}

pub fn parse_byte_range(header: Option<&str>, size: u64) -> ByteRange {
    let Some(header) = header else {
        return ByteRange::Full;
    };
    let header = header.trim();
    let Some(value) = header.strip_prefix("bytes=") else {
        return ByteRange::Invalid;
    };
    if value.contains(',') {
        return ByteRange::Invalid;
    }
    let Some((start_text, end_text)) = value.split_once('-') else {
        return ByteRange::Invalid;
    };
    if start_text.is_empty() && end_text.is_empty() {
        return ByteRange::Invalid;
    }
    let (start, end) = if start_text.is_empty() {
        let Ok(suffix) = end_text.parse::<u64>() else {
            return ByteRange::Invalid;
        };
        if suffix == 0 || size == 0 {
            return ByteRange::Invalid;
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let Ok(start) = start_text.parse::<u64>() else {
            return ByteRange::Invalid;
        };
        let end = if end_text.is_empty() {
            size.saturating_sub(1)
        } else {
            match end_text.parse::<u64>() {
                Ok(end) => end.min(size.saturating_sub(1)),
                Err(_) => return ByteRange::Invalid,
            }
        };
        if start >= size || start > end {
            return ByteRange::Invalid;
        }
        (start, end)
    };
    if start > end {
        return ByteRange::Invalid;
    }
    ByteRange::Partial { start, end }
}

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(metadata) = entry.metadata() {
                total += metadata.len();
            }
        }
    }
    total
}

pub fn file_mtime_seconds(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

const QUERY_SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub fn quote_query(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, QUERY_SAFE).to_string()
}

pub fn truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_f64().map(|v| v != 0.0).unwrap_or(true),
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
    }
}
