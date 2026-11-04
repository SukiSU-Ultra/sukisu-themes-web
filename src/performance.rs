use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Extensions, HeaderMap, Request, StatusCode, Version};
use axum::middleware::Next;
use axum::response::Response;
use futures::stream::poll_fn;
use futures::Stream;
use serde_json::{json, Value};
use tower_http::compression::{CompressionLayer, CompressionLevel, Predicate};

use crate::api::AppState;
use crate::config::PerformanceSettings;

const CATEGORY_HTML: usize = 0;
const CATEGORY_JS: usize = 1;
const CATEGORY_JSON: usize = 2;
const CATEGORY_SVG: usize = 3;
const CATEGORY_OTHER: usize = 4;
const CATEGORY_NAMES: [&str; 5] = ["html", "js", "json", "svg", "other"];

#[derive(Default)]
struct CategoryStats {
    compressed: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

pub struct CompressionStats {
    responses: AtomicU64,
    compressed: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    gzip: AtomicU64,
    zstd: AtomicU64,
    categories: [CategoryStats; 5],
}

impl Default for CompressionStats {
    fn default() -> Self {
        Self {
            responses: AtomicU64::new(0),
            compressed: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            gzip: AtomicU64::new(0),
            zstd: AtomicU64::new(0),
            categories: std::array::from_fn(|_| CategoryStats::default()),
        }
    }
}

impl CompressionStats {
    fn record_compressed(&self, encoding: &str, category: usize, bytes_in: u64, bytes_out: u64) {
        self.bytes_in.fetch_add(bytes_in, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes_out, Ordering::Relaxed);
        match encoding {
            "gzip" => {
                self.gzip.fetch_add(1, Ordering::Relaxed);
            }
            "zstd" => {
                self.zstd.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        if let Some(item) = self.categories.get(category) {
            item.compressed.fetch_add(1, Ordering::Relaxed);
            item.bytes_in.fetch_add(bytes_in, Ordering::Relaxed);
            item.bytes_out.fetch_add(bytes_out, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> Value {
        let responses = self.responses.load(Ordering::Relaxed);
        let compressed = self.compressed.load(Ordering::Relaxed);
        let bytes_in = self.bytes_in.load(Ordering::Relaxed);
        let bytes_out = self.bytes_out.load(Ordering::Relaxed);
        let categories = CATEGORY_NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let item = &self.categories[index];
                let input = item.bytes_in.load(Ordering::Relaxed);
                let output = item.bytes_out.load(Ordering::Relaxed);
                json!({
                    "name": name,
                    "compressed": item.compressed.load(Ordering::Relaxed),
                    "bytesIn": input,
                    "bytesOut": output,
                    "ratio": ratio(output, input),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "responses": responses,
            "compressed": compressed,
            "bytesIn": bytes_in,
            "bytesOut": bytes_out,
            "savedBytes": bytes_in.saturating_sub(bytes_out),
            "ratio": ratio(bytes_out, bytes_in),
            "gzip": self.gzip.load(Ordering::Relaxed),
            "zstd": self.zstd.load(Ordering::Relaxed),
            "categories": categories,
        })
    }

    pub fn reset(&self) {
        self.responses.store(0, Ordering::Relaxed);
        self.compressed.store(0, Ordering::Relaxed);
        self.bytes_in.store(0, Ordering::Relaxed);
        self.bytes_out.store(0, Ordering::Relaxed);
        self.gzip.store(0, Ordering::Relaxed);
        self.zstd.store(0, Ordering::Relaxed);
        for item in &self.categories {
            item.compressed.store(0, Ordering::Relaxed);
            item.bytes_in.store(0, Ordering::Relaxed);
            item.bytes_out.store(0, Ordering::Relaxed);
        }
    }
}

fn ratio(output: u64, input: u64) -> f64 {
    if input == 0 {
        0.0
    } else {
        output as f64 / input as f64
    }
}

pub struct PerformanceRuntime {
    pub compression_enabled: AtomicBool,
    pub compression_min_bytes: AtomicUsize,
    pub download_buffer_bytes: AtomicUsize,
    pub flush_interval_secs: AtomicU64,
    pub stats: CompressionStats,
    pub live: LiveStats,
}

pub struct TrafficMeter {
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl Default for TrafficMeter {
    fn default() -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
        }
    }
}

impl TrafficMeter {
    pub fn record(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let Ok(mut samples) = self.samples.lock() else {
            return;
        };
        let now = Instant::now();
        samples.push_back((now, bytes));
        while samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(5))
        {
            samples.pop_front();
        }
    }

    pub fn bytes_per_sec(&self) -> u64 {
        let Ok(samples) = self.samples.lock() else {
            return 0;
        };
        let now = Instant::now();
        let total: u64 = samples
            .iter()
            .filter(|(at, _)| now.duration_since(*at) <= Duration::from_secs(5))
            .map(|(_, bytes)| *bytes)
            .sum();
        total / 5
    }
}

pub struct LiveStats {
    pub active_requests: AtomicU64,
    pub active_assets: AtomicU64,
    pub active_uploads: AtomicU64,
    active_resources: Mutex<HashMap<String, u32>>,
    pub download: TrafficMeter,
    pub upload: TrafficMeter,
}

impl Default for LiveStats {
    fn default() -> Self {
        Self {
            active_requests: AtomicU64::new(0),
            active_assets: AtomicU64::new(0),
            active_uploads: AtomicU64::new(0),
            active_resources: Mutex::new(HashMap::new()),
            download: TrafficMeter::default(),
            upload: TrafficMeter::default(),
        }
    }
}

pub struct UploadGuard<'a> {
    stats: &'a LiveStats,
}

impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        self.stats.active_uploads.fetch_sub(1, Ordering::Relaxed);
    }
}

impl LiveStats {
    pub fn begin_asset(&self, path: &str) {
        self.active_assets.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut resources) = self.active_resources.lock() {
            if resources.len() > 512 {
                resources.clear();
            }
            *resources.entry(path.to_string()).or_insert(0) += 1;
        }
    }

    pub fn end_asset(&self, path: &str) {
        self.active_assets.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut resources) = self.active_resources.lock() {
            if let Some(count) = resources.get_mut(path) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    resources.remove(path);
                }
            }
        }
    }

    pub fn upload_guard(&self) -> UploadGuard<'_> {
        self.active_uploads.fetch_add(1, Ordering::Relaxed);
        UploadGuard { stats: self }
    }

    pub fn resources(&self) -> Vec<(String, u32)> {
        let mut values = self
            .active_resources
            .lock()
            .map(|resources| {
                resources
                    .iter()
                    .map(|(path, count)| (path.clone(), *count))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        values
    }
}

impl PerformanceRuntime {
    pub fn new(settings: &PerformanceSettings) -> Self {
        Self {
            compression_enabled: AtomicBool::new(settings.compression_enabled),
            compression_min_bytes: AtomicUsize::new(settings.compression_min_bytes),
            download_buffer_bytes: AtomicUsize::new(settings.download_buffer_bytes),
            flush_interval_secs: AtomicU64::new(settings.flush_interval_secs),
            stats: CompressionStats::default(),
            live: LiveStats::default(),
        }
    }

    pub fn apply_hot_settings(&self, settings: &PerformanceSettings) {
        self.compression_enabled
            .store(settings.compression_enabled, Ordering::Relaxed);
        self.compression_min_bytes
            .store(settings.compression_min_bytes, Ordering::Relaxed);
        self.download_buffer_bytes
            .store(settings.download_buffer_bytes, Ordering::Relaxed);
        self.flush_interval_secs
            .store(settings.flush_interval_secs, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct CompressionMeta {
    original_bytes: Arc<AtomicU64>,
    category: usize,
}

fn compression_level(value: &str) -> CompressionLevel {
    match value {
        "fastest" => CompressionLevel::Fastest,
        "best" => CompressionLevel::Best,
        _ => CompressionLevel::Default,
    }
}

pub fn is_compressible_type(value: &str) -> bool {
    let mime = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/javascript"
        || mime == "application/ecmascript"
        || mime == "application/xml"
        || mime == "application/manifest+json"
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || mime == "image/svg+xml"
}

fn category_for(content_type: Option<&str>) -> usize {
    let mime = content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime == "text/html" || mime == "application/xhtml+xml" {
        CATEGORY_HTML
    } else if mime.contains("javascript") || mime.contains("ecmascript") {
        CATEGORY_JS
    } else if mime.contains("json") {
        CATEGORY_JSON
    } else if mime == "image/svg+xml" {
        CATEGORY_SVG
    } else {
        CATEGORY_OTHER
    }
}

fn should_compress(runtime: &PerformanceRuntime, status: StatusCode, headers: &HeaderMap) -> bool {
    if !runtime.compression_enabled.load(Ordering::Relaxed) {
        return false;
    }
    if matches!(
        status,
        StatusCode::PARTIAL_CONTENT | StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
    ) {
        return false;
    }
    if headers.contains_key(header::CONTENT_ENCODING) {
        return false;
    }
    if headers
        .get(header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("no-transform"))
    {
        return false;
    }
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    if !is_compressible_type(content_type) {
        return false;
    }
    let min_bytes = runtime.compression_min_bytes.load(Ordering::Relaxed) as u64;
    if let Some(length) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        if length < min_bytes {
            return false;
        }
    }
    true
}

/// Compression layer built once at startup. Codec choices and quality are
/// captured here; enabling/disabling compression and the minimum size are read
/// from `PerformanceRuntime` on every response.
pub fn compression_layer(
    runtime: Arc<PerformanceRuntime>,
    settings: &PerformanceSettings,
) -> CompressionLayer<impl Predicate> {
    let predicate_runtime = runtime;
    CompressionLayer::new()
        .gzip(settings.compression_gzip)
        .zstd(settings.compression_zstd)
        .quality(compression_level(&settings.compression_quality))
        .compress_when(
            move |status: StatusCode, _version: Version, headers: &HeaderMap, _ext: &Extensions| {
                should_compress(&predicate_runtime, status, headers)
            },
        )
}

fn counted_body(body: Body, counter: Arc<AtomicU64>) -> Body {
    let mut stream = Box::pin(body.into_data_stream());
    Body::from_stream(poll_fn(move |cx| match stream.as_mut().poll_next(cx) {
        Poll::Ready(Some(Ok(bytes))) => {
            counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            Poll::Ready(Some(Ok(bytes)))
        }
        other => other,
    }))
}

/// Runs inside the compression layer and counts the bytes the handler produced.
pub async fn compression_inner_metrics(
    State(_state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() == StatusCode::PARTIAL_CONTENT {
        return response;
    }
    let Some(content_type) = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
    else {
        return response;
    };
    if !is_compressible_type(&content_type) {
        return response;
    }
    let counter = Arc::new(AtomicU64::new(0));
    let meta = CompressionMeta {
        original_bytes: counter.clone(),
        category: category_for(Some(&content_type)),
    };
    let (parts, body) = response.into_parts();
    let mut response = Response::from_parts(parts, counted_body(body, counter));
    response.extensions_mut().insert(meta);
    response
}

/// Runs outside the compression layer and counts the wire bytes after encoding.
pub async fn compression_outer_metrics(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    state
        .performance
        .stats
        .responses
        .fetch_add(1, Ordering::Relaxed);
    let Some(encoding) = response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
    else {
        return response;
    };
    if encoding.is_empty() || encoding == "identity" {
        return response;
    }
    state
        .performance
        .stats
        .compressed
        .fetch_add(1, Ordering::Relaxed);
    let meta = response.extensions().get::<CompressionMeta>().cloned();
    let category = meta.as_ref().map(|item| item.category).unwrap_or(CATEGORY_OTHER);
    let original = meta.as_ref().map(|item| item.original_bytes.clone());
    let runtime = state.performance.clone();
    let encoded_bytes = Arc::new(AtomicU64::new(0));
    let counter = encoded_bytes.clone();
    let (parts, body) = response.into_parts();
    let mut stream = Box::pin(body.into_data_stream());
    let mut finished = false;
    let body = Body::from_stream(poll_fn(move |cx| {
        match stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(None) => {
                if !finished {
                    finished = true;
                    let bytes_out = encoded_bytes.load(Ordering::Relaxed);
                    let bytes_in = original
                        .as_ref()
                        .map(|value| value.load(Ordering::Relaxed))
                        .unwrap_or(0);
                    runtime
                        .stats
                        .record_compressed(&encoding, category, bytes_in, bytes_out);
                }
                Poll::Ready(None)
            }
            other => other,
        }
    }));
    Response::from_parts(parts, body)
}

fn is_asset_path(path: &str) -> bool {
    path.starts_with("/themes/")
        || path.starts_with("/previews/")
        || path.starts_with("/screenshots/")
        || path.starts_with("/avatars/")
        || path == "/catalog.json"
}

pub async fn live_request_metrics(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let tracked = is_asset_path(&path);
    state
        .performance
        .live
        .active_requests
        .fetch_add(1, Ordering::Relaxed);
    if tracked {
        state.performance.live.begin_asset(&path);
    }
    let response = next.run(request).await;
    if tracked {
        if method != axum::http::Method::HEAD {
            if let Some(length) = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
            {
                state.performance.live.download.record(length);
            }
        }
        state.performance.live.end_asset(&path);
    }
    state
        .performance
        .live
        .active_requests
        .fetch_sub(1, Ordering::Relaxed);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn only_text_like_types_are_compressible() {
        for value in [
            "text/html; charset=utf-8",
            "text/css",
            "application/javascript",
            "application/json",
            "application/manifest+json",
            "application/xml",
            "image/svg+xml",
        ] {
            assert!(is_compressible_type(value), "{value}");
        }
        for value in [
            "image/jpeg",
            "image/png",
            "image/webp",
            "application/zip",
            "application/octet-stream",
            "font/woff2",
            "video/mp4",
        ] {
            assert!(!is_compressible_type(value), "{value}");
        }
    }

    #[test]
    fn compression_predicate_skips_ranges_images_and_small_bodies() {
        let runtime = Arc::new(PerformanceRuntime::new(&PerformanceSettings::default()));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("1024"));
        assert!(should_compress(&runtime, StatusCode::OK, &headers));
        assert!(!should_compress(
            &runtime,
            StatusCode::PARTIAL_CONTENT,
            &headers
        ));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/png"),
        );
        assert!(!should_compress(&runtime, StatusCode::OK, &headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("10"));
        assert!(!should_compress(&runtime, StatusCode::OK, &headers));

        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("1024"));
        runtime
            .compression_enabled
            .store(false, Ordering::Relaxed);
        assert!(!should_compress(&runtime, StatusCode::OK, &headers));
    }

    #[test]
    fn live_stats_track_active_resources_and_traffic() {
        let stats = LiveStats::default();
        {
            let _guard = stats.upload_guard();
            stats
                .active_requests
                .fetch_add(1, Ordering::Relaxed);
            assert_eq!(stats.active_uploads.load(Ordering::Relaxed), 1);
        }
        assert_eq!(stats.active_uploads.load(Ordering::Relaxed), 0);

        stats.begin_asset("/themes/a.sukisu");
        stats.begin_asset("/themes/a.sukisu");
        assert_eq!(stats.active_assets.load(Ordering::Relaxed), 2);
        assert_eq!(stats.resources()[0].1, 2);
        stats.end_asset("/themes/a.sukisu");
        stats.end_asset("/themes/a.sukisu");
        assert!(stats.resources().is_empty());

        stats.download.record(5_000);
        assert!(stats.download.bytes_per_sec() > 0);
    }
}

