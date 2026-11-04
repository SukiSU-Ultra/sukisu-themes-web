//! Server-side mirror selection for theme package downloads.
//!
//! The manager stays a plain HTTP client. For a package request without a
//! `Range` header, the server checks configured HTTP mirrors in priority order
//! and answers `302 Location: <mirror>/themes/...` when one of them actually
//! has the file. If no mirror is available it serves the package locally, as
//! before. Resumed downloads (`Range`) always stay on this server so a partial
//! file is never mixed between different origins.
//!
//! Configuration lives in `THEME_MIRRORS_FILE` (default
//! `/etc/sukisu-theme-web/mirrors.json`) and is reloaded every 30 seconds:
//!
//! ```json
//! {
//!   "mirrors": [
//!     {
//!       "name": "edge-a",
//!       "baseUrl": "https://a.example.com/",
//!       "token": "",
//!       "priority": 10,
//!       "enabled": true
//!     }
//!   ]
//! }
//! ```
//!
//! Mirrors must serve the same `themes/<id>-v<version>.sukisu` paths and be
//! byte-identical to this server. The manager forwards its
//! `X-Sukisu-Theme-Token` header across the redirect; a mirror either has to
//! accept that token or be publicly readable. `token` in the config is only
//! used for the server-side availability probe.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use axum::http::HeaderMap;
use reqwest::header::RANGE;
use reqwest::{Client, StatusCode};

use crate::config::TOKEN_HEADER;

const DEFAULT_MIRRORS_FILE: &str = "/etc/sukisu-theme-web/mirrors.json";
const MAX_MIRRORS: usize = 16;
const PATH_CACHE_TTL: Duration = Duration::from_secs(60);
const PATH_FAIL_TTL: Duration = Duration::from_secs(20);
const CHECK_TIMEOUT: Duration = Duration::from_millis(1_500);
const TOTAL_BUDGET: Duration = Duration::from_secs(4);
/// Added to the redirect target. If the "mirror" is actually this server, the
/// asset handler sees the marker and serves the file locally instead of
/// redirecting again, so a misconfigured self-mirror cannot loop.
const MIRROR_MARKER: &str = "sukisu_mirror=1";

#[derive(Clone, Debug)]
struct Mirror {
    name: String,
    base_url: String,
    token: String,
    priority: i32,
}

#[derive(Default)]
struct MirrorState {
    mirrors: Vec<Mirror>,
    modified: Option<SystemTime>,
    /// `base|path` -> (checked_at, available)
    checks: HashMap<String, (Instant, bool)>,
}

#[derive(Clone)]
pub struct MirrorPool {
    path: PathBuf,
    state: Arc<RwLock<MirrorState>>,
    client: Client,
    /// Rotates mirrors that share the same priority.
    cursor: Arc<AtomicUsize>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MirrorFile {
    #[serde(default)]
    mirrors: Vec<MirrorEntry>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MirrorEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    token: String,
    #[serde(default = "default_priority")]
    priority: i32,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_priority() -> i32 {
    100
}

fn default_enabled() -> bool {
    true
}

fn mirrors_path() -> PathBuf {
    std::env::var("THEME_MIRRORS_FILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MIRRORS_FILE))
}

impl MirrorPool {
    pub async fn load() -> Self {
        let path = mirrors_path();
        let client = Client::builder()
            .timeout(CHECK_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .unwrap_or_else(|_| Client::new());
        let pool = Self {
            path,
            state: Arc::new(RwLock::new(MirrorState::default())),
            client,
            cursor: Arc::new(AtomicUsize::new(0)),
        };
        pool.reload().await;
        pool
    }

    /// Re-read the config when the file's mtime changed. Called at startup and
    /// by the background maintenance loop.
    pub async fn reload(&self) {
        let metadata = match tokio::fs::metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(_) => {
                if let Ok(mut state) = self.state.write() {
                    if !state.mirrors.is_empty() || state.modified.is_some() {
                        *state = MirrorState::default();
                    }
                }
                return;
            }
        };
        let modified = metadata.modified().ok();
        {
            let state = match self.state.read() {
                Ok(state) => state,
                Err(_) => return,
            };
            if state.modified == modified {
                return;
            }
        }
        let raw = match tokio::fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(_) => return,
        };
        let parsed: MirrorFile = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(path = %self.path.display(), "ignoring invalid mirrors.json: {error}");
                return;
            }
        };
        let mut mirrors = Vec::new();
        for entry in parsed.mirrors.into_iter().take(MAX_MIRRORS) {            if !entry.enabled {
                continue;
            }
            let Some(base_url) = normalize_base_url(&entry.base_url) else {
                tracing::warn!(name = %entry.name, url = %entry.base_url, "ignoring invalid mirror URL");
                continue;
            };
            mirrors.push(Mirror {
                name: if entry.name.trim().is_empty() {
                    base_url.clone()
                } else {
                    entry.name.trim().chars().take(48).collect()
                },
                base_url,
                token: entry.token.trim().to_string(),
                priority: entry.priority,
            });
        }
        mirrors.sort_by(|left, right| left.base_url.cmp(&right.base_url));
        mirrors.dedup_by(|left, right| left.base_url == right.base_url);
        mirrors.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.name.cmp(&right.name))
        });
        if let Ok(mut state) = self.state.write() {
            *state = MirrorState {
                mirrors,
                modified,
                checks: HashMap::new(),
            };
        }
    }

    /// Return a redirect target for a package path if a healthy mirror has it.
    /// `Range` requests intentionally stay local.
    pub async fn pick(&self, path: &str, headers: &HeaderMap) -> Option<String> {
        if !path.starts_with("themes/") || headers.contains_key(RANGE) {
            return None;
        }
        let mirrors = {
            let state = self.state.read().ok()?;
            state.mirrors.clone()
        };
        if mirrors.is_empty() {
            return None;
        }
        // Keep priority order, but rotate mirrors that share a priority so one
        // edge does not receive every download.
        let mut ordered = Vec::with_capacity(mirrors.len());
        let mut index = 0;
        while index < mirrors.len() {
            let priority = mirrors[index].priority;
            let end = index
                + mirrors[index..]
                    .iter()
                    .take_while(|mirror| mirror.priority == priority)
                    .count();
            let group = &mirrors[index..end];
            let start = self.cursor.fetch_add(1, Ordering::Relaxed) % group.len();
            for offset in 0..group.len() {
                ordered.push(group[(start + offset) % group.len()].clone());
            }
            index = end;
        }
        let started = Instant::now();
        for mirror in ordered {
            if started.elapsed() > TOTAL_BUDGET {
                break;
            }
            let target = join_url(&mirror.base_url, path);
            let key = format!("{}|{}", mirror.base_url, path);
            if let Some((checked_at, available)) = self.cached(&key) {
                if available && checked_at.elapsed() < PATH_CACHE_TTL {
                    return Some(target);
                }
                if !available && checked_at.elapsed() < PATH_FAIL_TTL {
                    continue;
                }
            }
            let available = mirror_available(&self.client, &mirror, &target).await;
            self.remember(&key, available);
            if available {
                tracing::debug!(mirror = %mirror.name, %path, "redirecting theme package");
                return Some(target);
            }
        }
        None
    }

    fn cached(&self, key: &str) -> Option<(Instant, bool)> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.checks.get(key).copied())
    }

    fn remember(&self, key: &str, available: bool) {
        if let Ok(mut state) = self.state.write() {
            if state.checks.len() > 4_096 {
                state.checks.retain(|_, (at, _)| at.elapsed() < PATH_CACHE_TTL);
            }
            state.checks.insert(key.to_string(), (Instant::now(), available));
        }
    }
}

fn normalize_base_url(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}?{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/'),
        MIRROR_MARKER
    )
}

/// HEAD is cheap and does not download the package. Some file servers do not
/// implement HEAD, so fall back to a one-byte Range GET.
async fn mirror_available(client: &Client, mirror: &Mirror, url: &str) -> bool {
    let mut request = client.head(url);
    if !mirror.token.is_empty() {
        request = request.header(TOKEN_HEADER, &mirror.token);
    }
    if let Ok(Ok(response)) = tokio::time::timeout(CHECK_TIMEOUT, request.send()).await {
        let status = response.status();
        if status.is_success() || status == StatusCode::PARTIAL_CONTENT {
            return true;
        }
        if status != StatusCode::METHOD_NOT_ALLOWED {
            return false;
        }
    }
    let mut request = client.get(url).header(RANGE, "bytes=0-0");
    if !mirror.token.is_empty() {
        request = request.header(TOKEN_HEADER, &mirror.token);
    }
    matches!(
        tokio::time::timeout(CHECK_TIMEOUT, request.send()).await,
        Ok(Ok(response)) if response.status().is_success()
            || response.status() == StatusCode::PARTIAL_CONTENT
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_rejects_mirror_urls() {
        assert_eq!(
            normalize_base_url("https://Mirror.Example/").as_deref(),
            Some("https://mirror.example")
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:8080/base/").as_deref(),
            Some("http://127.0.0.1:8080/base")
        );
        assert!(normalize_base_url("ftp://mirror.example/").is_none());
        assert!(normalize_base_url("https://mirror.example/?x=1").is_none());
        assert!(normalize_base_url("").is_none());
    }

    #[test]
    fn joins_paths_without_double_slashes() {
        assert_eq!(
            join_url("https://mirror.example/base/", "/themes/a.sukisu"),
            "https://mirror.example/base/themes/a.sukisu?sukisu_mirror=1"
        );
    }

    #[tokio::test]
    async fn range_requests_are_not_redirected() {
        let mut state = MirrorState::default();
        state.mirrors.push(Mirror {
            name: "test".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            token: String::new(),
            priority: 1,
        });
        let pool = MirrorPool {
            path: PathBuf::from("/nonexistent"),
            state: Arc::new(RwLock::new(state)),
            client: Client::new(),
            cursor: Arc::new(AtomicUsize::new(0)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(RANGE, "bytes=10-".parse().unwrap());
        assert!(pool.pick("themes/a.sukisu", &headers).await.is_none());
    }
}
