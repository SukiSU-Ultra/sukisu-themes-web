//! Real-time API usage monitoring for the admin console.
//!
//! Every request under `/api`, `/admin/api`, `/account/api`, `/submit` and
//! `/catalog.json` is tracked while it is in flight and persisted to
//! `audit.api_request_logs` when it completes. The middleware captures request
//! and response headers (including cookies) plus a size-limited copy of both
//! bodies, identifies the caller from the admin/user session cookies or the
//! theme access token, and exposes an abort channel that cancels the in-flight
//! future and revokes the caller's session.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use futures::{Stream, StreamExt};
use rusqlite::types::Value as SqlValue;
use serde::Deserialize;
use serde_json::{json, map::Map, Value};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, watch};

use crate::accounts::SessionInfo;
use crate::api::{ApiError, AppState};
use crate::config::{read_env, Paths};
use crate::db::Database;

type ApiResult<T> = Result<T, ApiError>;

const DEFAULT_BODY_LIMIT: usize = 128 * 1024;
const DEFAULT_CAPTURE_BUDGET: usize = 32 * 1024 * 1024;
const DEFAULT_RETENTION: i64 = 2_000_000;
const MAX_EXPORT_BATCH: i64 = 200;
const MAX_DELETE_IDS: usize = 500;
const RECENT_LIMIT: usize = 100;

const INSERT_SQL: &str = "INSERT INTO audit.api_request_logs(
    connection_id, method, path, query, function, status, user_id, username, display_name,
    auth_kind, ip, user_agent, request_headers, request_content_type, request_body,
    request_bytes, request_truncated, response_headers, response_content_type, response_body,
    response_bytes, response_truncated, started_at, ended_at, duration_ms, aborted, error,
    user_role, user_status, email_verified)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30)";

const SUMMARY_COLUMNS: &str = "id, connection_id, method, path, query, function, status, user_id,
    username, display_name, auth_kind, ip, request_bytes, response_bytes, started_at, ended_at,
    duration_ms, aborted, error";

const FULL_COLUMNS: &str = "id, connection_id, method, path, query, function, status, user_id,
    username, display_name, auth_kind, ip, user_agent, request_headers, request_content_type,
    request_body, request_bytes, request_truncated, response_headers, response_content_type,
    response_body, response_bytes, response_truncated, started_at, ended_at, duration_ms, aborted,
    error, user_role, user_status, email_verified";

#[derive(Clone, Debug)]
struct MonitorSettings {
    enabled: bool,
    body_limit: usize,
    capture_budget: usize,
    retention: i64,
}

impl MonitorSettings {
    fn load(paths: &Paths) -> Self {
        let env = read_env(&paths.admin_config);
        let enabled = env
            .get("API_MONITOR_ENABLED")
            .map(|value| parse_bool(value, true))
            .unwrap_or(true);
        let body_limit = env
            .get("API_MONITOR_BODY_LIMIT")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_BODY_LIMIT)
            .clamp(16 * 1024, 4 * 1024 * 1024);
        let capture_budget = env
            .get("API_MONITOR_CAPTURE_BUDGET")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(DEFAULT_CAPTURE_BUDGET)
            .clamp(1024 * 1024, 512 * 1024 * 1024);
        let retention = env
            .get("API_MONITOR_RETENTION")
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_RETENTION)
            .max(0);
        Self {
            enabled,
            body_limit,
            capture_budget,
            retention,
        }
    }
}

fn parse_bool(value: &str, fallback: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

#[derive(Default)]
struct Stats {
    total: u64,
    aborted: u64,
    dropped: u64,
}

/// Shared ceiling for body bytes held by in-flight connections. Without this,
/// a flood of slow uploads could each retain a full per-connection capture.
/// Reservations are released when the capture is dropped.
struct CaptureBudget {
    used: AtomicUsize,
    limit: usize,
}

impl CaptureBudget {
    fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    fn reserve_up_to(&self, amount: usize) -> usize {
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let available = self.limit.saturating_sub(current);
            if available == 0 {
                return 0;
            }
            let granted = amount.min(available);
            match self.used.compare_exchange_weak(
                current,
                current + granted,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return granted,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, amount: usize) {
        if amount > 0 {
            self.used.fetch_sub(amount, Ordering::Relaxed);
        }
    }

    fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
}

pub struct MonitorRuntime {
    enabled: bool,
    body_limit: usize,
    retention: i64,
    capture_budget: Arc<CaptureBudget>,
    db: Database,
    active: Mutex<HashMap<String, Arc<Connection>>>,
    recent: Mutex<VecDeque<Value>>,
    stats: Mutex<Stats>,
    last24h: Mutex<Option<(Instant, u64)>>,
    token_names: Mutex<HashMap<String, (Option<String>, Instant)>>,
    tx: mpsc::Sender<ApiLogRecord>,
}

impl MonitorRuntime {
    pub async fn new(db: Database, paths: &Paths) -> Arc<Self> {
        let settings = MonitorSettings::load(paths);
        let (tx, rx) = mpsc::channel(512);
        let total = db
            .call(|conn| {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM audit.api_request_logs", [], |row| {
                        row.get(0)
                    })?;
                Ok(count)
            })
            .await
            .unwrap_or(0)
            .max(0) as u64;
        let runtime = Arc::new(Self {
            enabled: settings.enabled,
            body_limit: settings.body_limit,
            retention: settings.retention,
            capture_budget: Arc::new(CaptureBudget::new(settings.capture_budget)),
            db,
            active: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            stats: Mutex::new(Stats {
                total,
                ..Stats::default()
            }),
            last24h: Mutex::new(None),
            token_names: Mutex::new(HashMap::new()),
            tx,
        });
        let writer = Arc::clone(&runtime);
        tokio::spawn(async move {
            writer.writer_loop(rx).await;
        });
        runtime
    }

    fn register(&self, connection: Arc<Connection>) {
        let Ok(mut active) = self.active.lock() else {
            return;
        };
        if active.len() >= 4096 {
            // Keep memory bounded if a client opens an extreme number of
            // requests. The oldest entries remain visible and abortable.
            if let Some(oldest) = active
                .iter()
                .min_by_key(|(_, item)| item.started)
                .map(|(id, _)| id.clone())
            {
                active.remove(&oldest);
            }
        }
        active.insert(connection.id.clone(), connection);
    }

    fn unregister(&self, id: &str) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(id);
        }
    }

    fn enqueue(&self, record: ApiLogRecord) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.total = stats.total.saturating_add(1);
            if record.aborted != 0 {
                stats.aborted = stats.aborted.saturating_add(1);
            }
        }
        if let Ok(mut recent) = self.recent.lock() {
            recent.push_front(record.summary_value());
            while recent.len() > RECENT_LIMIT {
                recent.pop_back();
            }
        }
        if let Err(error) = self.tx.try_send(record) {
            if matches!(error, mpsc::error::TrySendError::Full(_)) {
                if let Ok(mut stats) = self.stats.lock() {
                    stats.dropped = stats.dropped.saturating_add(1);
                    stats.total = stats.total.saturating_sub(1);
                }
            }
        }
    }

    fn abort(&self, id: &str) -> Option<Arc<Connection>> {
        let connection = {
            let Ok(mut active) = self.active.lock() else {
                return None;
            };
            active.remove(id)?
        };
        connection.abort_tx.send_replace(true);
        Some(connection)
    }

    fn active_connection(&self, id: &str) -> Option<Arc<Connection>> {
        self.active
            .lock()
            .ok()
            .and_then(|active| active.get(id).cloned())
    }

    fn active_list(&self) -> Vec<Value> {
        let mut items = self
            .active
            .lock()
            .map(|active| {
                active
                    .values()
                    .map(|connection| connection.summary_value())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        items.sort_by_key(|item| {
            item.get("startedAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        });
        items.reverse();
        items
    }

    fn recent_list(&self) -> Vec<Value> {
        self.recent
            .lock()
            .map(|recent| recent.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn stats_snapshot(&self) -> Value {
        let stats = self
            .stats
            .lock()
            .map(|stats| Stats {
                total: stats.total,
                aborted: stats.aborted,
                dropped: stats.dropped,
            })
            .unwrap_or_default();
        let active = self.active.lock().map(|active| active.len()).unwrap_or(0);
        json!({
            "enabled": self.enabled,
            "active": active,
            "total": stats.total,
            "aborted": stats.aborted,
            "dropped": stats.dropped,
            "bodyLimit": self.body_limit,
            "retention": self.retention,
            "captureUsed": self.capture_budget.used(),
            "captureBudget": self.capture_budget.limit,
        })
    }

    pub async fn last_24h(&self) -> u64 {
        if let Ok(cache) = self.last24h.lock() {
            if let Some((at, value)) = *cache {
                if at.elapsed() < Duration::from_secs(30) {
                    return value;
                }
            }
        }
        let since =
            (Utc::now() - ChronoDuration::hours(24)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let db = self.db.clone();
        let count = db
            .call(move |conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM audit.api_request_logs WHERE started_at >= ?1",
                    [since],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .await
            .unwrap_or(0)
            .max(0) as u64;
        if let Ok(mut cache) = self.last24h.lock() {
            *cache = Some((Instant::now(), count));
        }
        count
    }

    async fn refresh_counts(&self) {
        let db = self.db.clone();
        if let Ok(count) = db
            .call(|conn| {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM audit.api_request_logs", [], |row| {
                        row.get(0)
                    })?;
                Ok(count)
            })
            .await
        {
            if let Ok(mut stats) = self.stats.lock() {
                stats.total = count.max(0) as u64;
            }
        }
        if let Ok(mut cache) = self.last24h.lock() {
            *cache = None;
        }
    }

    /// Cache token names briefly; image/package downloads can otherwise cause
    /// one SQLite lookup per request just to label the caller.
    async fn theme_token_name(&self, token: &str) -> Option<String> {
        const TTL: Duration = Duration::from_secs(60);
        let key = crate::accounts::hash_token(token);
        if let Ok(cache) = self.token_names.lock() {
            if let Some((name, at)) = cache.get(&key) {
                if at.elapsed() < TTL {
                    return name.clone();
                }
            }
        }
        let name = crate::theme_tokens::name_for_token(&self.db, token).await;
        if let Ok(mut cache) = self.token_names.lock() {
            if cache.len() > 4096 {
                cache.clear();
            }
            cache.insert(key, (name.clone(), Instant::now()));
        }
        name
    }

    async fn writer_loop(self: Arc<Self>, mut rx: mpsc::Receiver<ApiLogRecord>) {
        let mut batch = Vec::new();
        let mut last_prune = Instant::now();
        loop {
            let Some(first) = rx.recv().await else {
                break;
            };
            batch.push(first);
            // Coalesce bursts into one transaction without delaying a lone log
            // for more than a moment.
            tokio::time::sleep(Duration::from_millis(80)).await;
            while batch.len() < 200 {
                match rx.try_recv() {
                    Ok(record) => batch.push(record),
                    Err(_) => break,
                }
            }
            if let Err(error) = self.insert_batch(&batch).await {
                tracing::warn!("api monitor log insert failed: {error:#}");
            }
            batch.clear();
            if last_prune.elapsed() >= Duration::from_secs(60) {
                last_prune = Instant::now();
                if self.retention > 0 {
                    match self.prune().await {
                        Ok(Some(remaining)) => {
                            if let Ok(mut stats) = self.stats.lock() {
                                stats.total = remaining;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => tracing::warn!("api monitor prune failed: {error:#}"),
                    }
                }
            }
        }
    }

    async fn insert_batch(&self, records: &[ApiLogRecord]) -> anyhow::Result<()> {
        let records = records.to_vec();
        self.db
            .call(move |conn| {
                let transaction = conn.transaction()?;
                for record in &records {
                    transaction.execute(
                        INSERT_SQL,
                        rusqlite::params![
                            record.connection_id,
                            record.method,
                            record.path,
                            record.query,
                            record.function,
                            record.status,
                            record.user_id,
                            record.username,
                            record.display_name,
                            record.auth_kind,
                            record.ip,
                            record.user_agent,
                            record.request_headers,
                            record.request_content_type,
                            record.request_body,
                            record.request_bytes,
                            record.request_truncated,
                            record.response_headers,
                            record.response_content_type,
                            record.response_body,
                            record.response_bytes,
                            record.response_truncated,
                            record.started_at,
                            record.ended_at,
                            record.duration_ms,
                            record.aborted,
                            record.error,
                            record.user_role,
                            record.user_status,
                            record.email_verified.map(i64::from),
                        ],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    async fn prune(&self) -> anyhow::Result<Option<u64>> {
        let retention = self.retention;
        self.db
            .call(move |conn| {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM audit.api_request_logs", [], |row| {
                        row.get(0)
                    })?;
                if count <= retention {
                    return Ok(None);
                }
                let remove = count - retention;
                conn.execute(
                    "DELETE FROM audit.api_request_logs WHERE id IN (
                        SELECT id FROM audit.api_request_logs ORDER BY id ASC LIMIT ?1
                    )",
                    [remove],
                )?;
                Ok(Some(retention.max(0) as u64))
            })
            .await
    }
}

struct Connection {
    id: String,
    method: String,
    path: String,
    query: String,
    function: String,
    ip: String,
    user_id: Option<String>,
    username: Option<String>,
    display_name: Option<String>,
    user_role: Option<String>,
    user_status: Option<String>,
    email_verified: Option<bool>,
    auth_kind: String,
    tokens: Vec<String>,
    user_agent: String,
    request_headers: String,
    request_content_type: String,
    request_capture: Mutex<BodyCapture>,
    response_capture: Mutex<BodyCapture>,
    response_status: AtomicU16,
    response_headers: Mutex<String>,
    started_at: String,
    started: Instant,
    abort_tx: watch::Sender<bool>,
    finished: AtomicBool,
}

impl Connection {
    fn is_aborted(&self) -> bool {
        *self.abort_tx.borrow()
    }

    fn push_request(&self, bytes: &[u8]) {
        if let Ok(mut capture) = self.request_capture.lock() {
            capture.push(bytes);
        }
    }

    fn push_response(&self, bytes: &[u8]) {
        if let Ok(mut capture) = self.response_capture.lock() {
            capture.push(bytes);
        }
    }

    fn set_response(&self, status: StatusCode, headers: &HeaderMap) {
        self.response_status.store(status.as_u16(), Ordering::Relaxed);
        if let Ok(mut value) = self.response_headers.lock() {
            *value = headers_to_json(headers, !is_credential_capture_path(&self.path));
        }
        let content_type = header_text(headers, "content-type");
        if let Ok(mut capture) = self.response_capture.lock() {
            capture.content_type = content_type;
        }
    }

    fn summary_value(&self) -> Value {
        let (request_bytes, response_bytes) = (
            self.request_capture
                .lock()
                .map(|capture| capture.total)
                .unwrap_or(0),
            self.response_capture
                .lock()
                .map(|capture| capture.total)
                .unwrap_or(0),
        );
        json!({
            "id": self.id,
            "connectionId": self.id,
            "method": self.method,
            "path": self.path,
            "query": self.query,
            "function": self.function,
            "status": self.response_status.load(Ordering::Relaxed),
            "userId": self.user_id,
            "username": self.username,
            "displayName": self.display_name,
            "userRole": self.user_role,
            "userStatus": self.user_status,
            "emailVerified": self.email_verified,
            "authKind": self.auth_kind,
            "ip": self.ip,
            "userAgent": self.user_agent,
            "startedAt": self.started_at,
            "elapsedMs": self.started.elapsed().as_millis() as u64,
            "requestBytes": request_bytes,
            "responseBytes": response_bytes,
            "aborted": self.is_aborted(),
        })
    }

    fn detail_value(&self) -> Value {
        let request_body = self
            .request_capture
            .lock()
            .map(|capture| capture.to_value())
            .unwrap_or(Value::Null);
        let response_body = self
            .response_capture
            .lock()
            .map(|capture| capture.to_value())
            .unwrap_or(Value::Null);
        let mut value = self.summary_value();
        if let Value::Object(map) = &mut value {
            map.insert(
                "requestHeaders".into(),
                parse_json_text(Some(self.request_headers.clone())),
            );
            map.insert(
                "responseHeaders".into(),
                parse_json_text(
                    self.response_headers
                        .lock()
                        .map(|headers| headers.clone())
                        .ok(),
                ),
            );
            map.insert(
                "requestContentType".into(),
                json!(self.request_content_type),
            );
            map.insert("requestBody".into(), request_body);
            map.insert("responseBody".into(), response_body);
            map.insert("sessions".into(), json!(self.tokens.len()));
        }
        value
    }

    fn complete(
        &self,
        monitor: &MonitorRuntime,
        status_override: Option<u16>,
        aborted: bool,
        error: Option<String>,
    ) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        monitor.unregister(&self.id);
        let status = status_override
            .map(i64::from)
            .unwrap_or_else(|| i64::from(self.response_status.load(Ordering::Relaxed)));
        let response_headers = self
            .response_headers
            .lock()
            .map(|headers| headers.clone())
            .unwrap_or_else(|_| "{}".to_string());
        let response_content_type = self
            .response_capture
            .lock()
            .map(|capture| capture.content_type.clone())
            .unwrap_or_default();
        let (request_body, request_bytes, request_truncated) = self
            .request_capture
            .lock()
            .map(|capture| {
                (
                    serde_json::to_string(&capture.to_value()).unwrap_or_else(|_| "null".into()),
                    capture.total as i64,
                    i64::from(capture.truncated),
                )
            })
            .unwrap_or_else(|_| ("null".to_string(), 0, 0));
        let (response_body, response_bytes, response_truncated) = self
            .response_capture
            .lock()
            .map(|capture| {
                (
                    serde_json::to_string(&capture.to_value()).unwrap_or_else(|_| "null".into()),
                    capture.total as i64,
                    i64::from(capture.truncated),
                )
            })
            .unwrap_or_else(|_| ("null".to_string(), 0, 0));
        let record = ApiLogRecord {
            connection_id: self.id.clone(),
            method: self.method.clone(),
            path: self.path.clone(),
            query: self.query.clone(),
            function: self.function.clone(),
            status,
            user_id: self.user_id.clone(),
            username: self.username.clone(),
            display_name: self.display_name.clone(),
            user_role: self.user_role.clone(),
            user_status: self.user_status.clone(),
            email_verified: self.email_verified,
            auth_kind: self.auth_kind.clone(),
            ip: self.ip.clone(),
            user_agent: self.user_agent.clone(),
            request_headers: self.request_headers.clone(),
            request_content_type: self.request_content_type.clone(),
            request_body,
            request_bytes,
            request_truncated,
            response_headers,
            response_content_type,
            response_body,
            response_bytes,
            response_truncated,
            started_at: self.started_at.clone(),
            ended_at: crate::util::now_iso(),
            duration_ms: self.started.elapsed().as_millis() as i64,
            aborted: i64::from(aborted),
            error,
        };
        monitor.enqueue(record);
    }
}

#[derive(Clone)]
struct ApiLogRecord {
    connection_id: String,
    method: String,
    path: String,
    query: String,
    function: String,
    status: i64,
    user_id: Option<String>,
    username: Option<String>,
    display_name: Option<String>,
    user_role: Option<String>,
    user_status: Option<String>,
    email_verified: Option<bool>,
    auth_kind: String,
    ip: String,
    user_agent: String,
    request_headers: String,
    request_content_type: String,
    request_body: String,
    request_bytes: i64,
    request_truncated: i64,
    response_headers: String,
    response_content_type: String,
    response_body: String,
    response_bytes: i64,
    response_truncated: i64,
    started_at: String,
    ended_at: String,
    duration_ms: i64,
    aborted: i64,
    error: Option<String>,
}

impl ApiLogRecord {
    fn summary_value(&self) -> Value {
        json!({
            "id": self.connection_id,
            "connectionId": self.connection_id,
            "method": self.method,
            "path": self.path,
            "query": self.query,
            "function": self.function,
            "status": self.status,
            "userId": self.user_id,
            "username": self.username,
            "displayName": self.display_name,
            "userRole": self.user_role,
            "userStatus": self.user_status,
            "emailVerified": self.email_verified,
            "authKind": self.auth_kind,
            "ip": self.ip,
            "userAgent": self.user_agent,
            "startedAt": self.started_at,
            "endedAt": self.ended_at,
            "durationMs": self.duration_ms,
            "requestBytes": self.request_bytes,
            "responseBytes": self.response_bytes,
            "aborted": self.aborted != 0,
        })
    }
}

struct BodyCapture {
    limit: usize,
    content_type: String,
    budget: Arc<CaptureBudget>,
    reserved: usize,
    bytes: Vec<u8>,
    total: u64,
    truncated: bool,
}

impl BodyCapture {
    fn new(limit: usize, content_type: String, budget: Arc<CaptureBudget>) -> Self {
        Self {
            limit,
            content_type,
            budget,
            reserved: 0,
            bytes: Vec::new(),
            total: 0,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        if chunk.is_empty() {
            return;
        }
        let room = self.limit.saturating_sub(self.bytes.len());
        let take = chunk.len().min(room);
        if take < chunk.len() {
            self.truncated = true;
        }
        if take == 0 {
            return;
        }
        let granted = self.budget.reserve_up_to(take);
        if granted < take {
            self.truncated = true;
        }
        if granted > 0 {
            self.bytes.extend_from_slice(&chunk[..granted]);
            self.reserved = self.reserved.saturating_add(granted);
        }
    }

    fn to_value(&self) -> Value {
        let content_type = self.content_type.clone();
        let textual = is_textual(&content_type)
            || (content_type.is_empty() && std::str::from_utf8(&self.bytes).is_ok());
        if self.bytes.is_empty() {
            return json!({
                "kind": "empty",
                "bytes": self.total,
                "truncated": self.truncated,
                "contentType": content_type,
            });
        }
        if textual {
            let text = String::from_utf8_lossy(&self.bytes).to_string();
            json!({
                "kind": "text",
                "text": text,
                "bytes": self.total,
                "truncated": self.truncated,
                "contentType": content_type,
            })
        } else {
            let preview = &self.bytes[..self.bytes.len().min(512)];
            json!({
                "kind": "binary",
                "bytes": self.total,
                "truncated": self.truncated,
                "contentType": content_type,
                "previewHex": hex::encode(preview),
            })
        }
    }
}

impl Drop for BodyCapture {
    fn drop(&mut self) {
        self.budget.release(self.reserved);
    }
}

struct MonitoredBody {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    connection: Arc<Connection>,
    monitor: Arc<MonitorRuntime>,
    done: bool,
}

impl Stream for MonitoredBody {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if this.connection.is_aborted() {
            this.done = true;
            this.connection.complete(
                &this.monitor,
                None,
                true,
                Some("管理员强制断开连接".to_string()),
            );
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for MonitoredBody {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            self.connection.complete(
                &self.monitor,
                None,
                true,
                Some("客户端在响应完成前断开".to_string()),
            );
        }
    }
}

/// Wraps a response body so an abort signal interrupts it even while the
/// underlying stream is blocked on a slow client (large theme downloads).
fn monitored_body(body: Body, connection: Arc<Connection>, monitor: Arc<MonitorRuntime>) -> Body {
    let stream_connection = Arc::clone(&connection);
    let stream_monitor = Arc::clone(&monitor);
    let inner = futures::stream::unfold(body.into_data_stream(), move |mut inner| {
        let connection = Arc::clone(&stream_connection);
        let monitor = Arc::clone(&stream_monitor);
        async move {
            let mut abort_rx = connection.abort_tx.subscribe();
            if connection.is_aborted() {
                connection.complete(&monitor, None, true, Some("管理员强制断开连接".to_string()));
                return None;
            }
            tokio::select! {
                changed = abort_rx.changed() => {
                    let aborted = connection.is_aborted();
                    let message = if aborted {
                        "管理员强制断开连接"
                    } else {
                        "响应流已关闭"
                    };
                    connection.complete(&monitor, None, aborted, Some(message.to_string()));
                    let _ = changed;
                    None
                }
                item = inner.next() => {
                    match item {
                        Some(Ok(bytes)) => {
                            connection.push_response(&bytes);
                            Some((Ok(bytes), inner))
                        }
                        Some(Err(error)) => {
                            let message = error.to_string();
                            connection.complete(&monitor, None, true, Some(message));
                            Some((Err(error), inner))
                        }
                        None => {
                            connection.complete(&monitor, None, false, None);
                            None
                        }
                    }
                }
            }
        }
    });
    Body::from_stream(MonitoredBody {
        inner: Box::pin(inner),
        connection,
        monitor,
        done: false,
    })
}

struct Identity {
    user_id: Option<String>,
    username: Option<String>,
    display_name: Option<String>,
    role: Option<String>,
    status: Option<String>,
    email_verified: Option<bool>,
    auth_kind: &'static str,
    tokens: Vec<String>,
}

impl Identity {
    fn anonymous() -> Self {
        Self {
            user_id: None,
            username: None,
            display_name: None,
            role: None,
            status: None,
            email_verified: None,
            auth_kind: "none",
            tokens: Vec::new(),
        }
    }
}

fn is_monitored_path(path: &str) -> bool {
    if path.starts_with("/client/")
        || path.starts_with("/_site/")
        || path.starts_with("/account/api/access")
        || path.starts_with("/api/client-credentials")
        || path.starts_with("/admin/api/client-credentials")
        || path.starts_with("/api/sites")
        || path.starts_with("/admin/api/sites")
    {
        return false;
    }
    // Upload status responses contain short-lived edge capabilities.
    if path.starts_with("/distribution/") || path.starts_with("/api/uploads/")
        || path.starts_with("/admin/api/uploads/") || path.starts_with("/account/api/uploads/") {
        return false;
    }
    // One-time node credentials must not be persisted in request/response captures.
    if path.starts_with("/admin/api/distribution") || path.starts_with("/api/distribution") {
        return false;
    }
    if path.starts_with("/admin/api/monitor") || path.starts_with("/api/monitor") {
        return false;
    }
    path == "/submit"
        || path == "/catalog.json"
        || path == "/health"
        || path == "/healthz"
        || path == "/api"
        || path.starts_with("/api/")
        || path == "/admin/api"
        || path.starts_with("/admin/api/")
        || path == "/account/api"
        || path.starts_with("/account/api/")
        || path.starts_with("/themes/")
        || path.starts_with("/previews/")
        || path.starts_with("/screenshots/")
        || path.starts_with("/avatars/")
}

fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Session tokens are 32 random bytes rendered as hex. Rejecting malformed
/// cookie values up front keeps arbitrary garbage from triggering SQLite
/// lookups on every monitored request.
fn plausible_session_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Routes whose bodies/credentials carry passwords, one-time tokens or
/// mail/storage secrets. They stay monitored, but their bodies and auth headers
/// must never be persisted to the audit database.
fn is_credential_capture_path(path: &str) -> bool {
    matches!(
        path,
        "/api/login"
            | "/admin/api/login"
            | "/api/setup"
            | "/admin/api/setup"
            | "/api/password"
            | "/admin/api/password"
            | "/account/api/password"
            | "/account/api/email"
            | "/account/api/auth/login"
            | "/account/api/auth/register"
            | "/account/api/auth/forgot-password"
            | "/account/api/auth/reset-password"
            | "/account/api/auth/verify-email"
            | "/account/api/auth/resend-verification"
            | "/account/api/auth/github/callback"
            | "/account/api/auth/device/start"
            | "/account/api/auth/device/poll"
            | "/account/api/auth/device/info"
            | "/account/api/auth/device/approve"
            | "/api/auth-settings"
            | "/admin/api/auth-settings"
            | "/api/auth-settings/test-email"
            | "/admin/api/auth-settings/test-email"
            | "/api/auth-settings/test-github"
            | "/admin/api/auth-settings/test-github"
            | "/api/tokens/create"
            | "/admin/api/tokens/create"
            | "/api/storage"
            | "/api/storage/backend"
    )
}

fn headers_to_json(headers: &HeaderMap, redact_credentials: bool) -> String {
    let mut map = Map::new();
    for (name, value) in headers.iter() {
        let key = name.as_str().to_string();
        let sensitive_header = matches!(
            key.as_str(),
            "cookie" | "set-cookie" | "authorization" | "x-sukisu-theme-token"
        );
        if (redact_credentials && sensitive_header)
            || value
                .to_str()
                .is_ok_and(|s| s.starts_with("sa_") || s.starts_with("sr_"))
            || matches!(key.as_str(), "x-sukisu-site-key" | "x-sukisu-site-internal")
        {
            map.insert(key, Value::String("[redacted]".into()));
            continue;
        }
        let text = value
            .to_str()
            .map(str::to_string)
            .unwrap_or_else(|_| format!("<{} bytes>", value.as_bytes().len()));
        if let Some(previous) = map.remove(&key) {
            let next = match previous {
                Value::Array(mut items) => {
                    items.push(Value::String(text));
                    Value::Array(items)
                }
                other => json!([other, text]),
            };
            map.insert(key, next);
        } else {
            map.insert(key, Value::String(text));
        }
    }
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

fn parse_json_text(value: Option<String>) -> Value {
    value
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn is_textual(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime == "application/x-www-form-urlencoded"
        || mime == "application/graphql"
        || mime == "application/javascript"
        || mime == "application/ecmascript"
}

fn identity_from_session(session: crate::accounts::SessionInfo, token: String) -> Identity {
    let auth_kind = if session.scope == "admin" {
        "admin"
    } else {
        "user"
    };
    Identity {
        user_id: Some(session.user_id),
        username: Some(session.username),
        display_name: Some(session.display_name),
        role: Some(session.role),
        status: Some(session.status),
        email_verified: Some(session.email_verified),
        auth_kind,
        tokens: vec![token],
    }
}

/// Optional headers a manager can use to carry the account session alongside
/// the theme token. They only affect monitor labelling; authorization still
/// comes from the theme token / normal session middleware.
fn session_header_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut tokens = Vec::new();
    for name in [
        "x-sukisu-user-token",
        "x-sukisu-account-token",
        "x-sukisu-session",
        "x-user-token",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            let value = value.trim();
            if plausible_session_token(value) && !tokens.iter().any(|item| item == value) {
                tokens.push(value.to_string());
            }
        }
    }
    tokens
}

async fn identify(state: &AppState, headers: &HeaderMap) -> Identity {
    if let Some(token) = crate::auth::session_token(headers, crate::auth::ADMIN_SESSION_COOKIE)
        .filter(|token| plausible_session_token(token))
    {
        if let Some(session) = state.sessions.validate(&token, "admin").await {
            return identity_from_session(session, token);
        }
    }
    if let Some(token) = crate::auth::session_token(headers, crate::auth::USER_SESSION_COOKIE)
        .filter(|token| plausible_session_token(token))
    {
        if let Some(session) = state.sessions.validate(&token, "user").await {
            return identity_from_session(session, token);
        }
    }
    if let Some(token) =
        crate::auth::bearer_token(headers).filter(|token| plausible_session_token(token))
    {
        if let Some(session) = state.sessions.validate(&token, "admin").await {
            return identity_from_session(session, token);
        }
        if let Some(session) = state.sessions.validate(&token, "user").await {
            return identity_from_session(session, token);
        }
    }
    for token in session_header_tokens(headers) {
        if let Some(session) = state.sessions.validate(&token, "user").await {
            return identity_from_session(session, token);
        }
        if let Some(session) = state.sessions.validate(&token, "admin").await {
            return identity_from_session(session, token);
        }
    }
    let provided = header_text(headers, crate::config::TOKEN_HEADER);
    if !provided.is_empty() {
        let display_name = if provided.starts_with("sa_") {
            Some("设备只读凭证".to_string())
        } else if plausible_session_token(&provided) {
            // Theme tokens are 32 random bytes as hex; reject arbitrary values
            // before they can trigger a database miss on every asset request.
            state.monitor.theme_token_name(&provided).await
        } else {
            None
        };
        return Identity {
            user_id: None,
            username: None,
            display_name,
            role: None,
            status: None,
            email_verified: None,
            auth_kind: "theme_token",
            tokens: Vec::new(),
        };
    }
    Identity::anonymous()
}

fn constant_time_eq(expected: &str, provided: &str) -> bool {
    !expected.is_empty()
        && expected.len() == provided.len()
        && expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

fn capture_request_body(body: Body, connection: Arc<Connection>) -> Body {
    let mut stream = Box::pin(body.into_data_stream());
    Body::from_stream(futures::stream::poll_fn(move |cx| {
        match stream.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                connection.push_request(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            other => other,
        }
    }))
}

fn aborted_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "aborted": true,
            "error": "连接已被管理员断开",
        })),
    )
        .into_response()
}

pub async fn monitor(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let monitor = Arc::clone(&state.monitor);
    let path = request.uri().path().to_string();
    if !monitor.enabled || !is_monitored_path(&path) {
        return next.run(request).await;
    }
    let method = request.method().as_str().to_string();
    let raw_query = request.uri().query().unwrap_or("").to_string();
    let credential_path = is_credential_capture_path(&path);
    // Query strings on these routes carry reset/verification/device codes.
    let query = if credential_path && !raw_query.is_empty() {
        "[redacted]".to_string()
    } else {
        raw_query
    };
    let headers = request.headers().clone();
    let (parts, body) = request.into_parts();

    let identity = identify(&state, &headers).await;
    let capture_bodies = !credential_path;
    let body_limit = if capture_bodies { monitor.body_limit } else { 0 };
    let capture_budget = Arc::clone(&monitor.capture_budget);
    let request_content_type = header_text(&headers, "content-type");
    let connection = Arc::new(Connection {
        id: crate::util::random_hex(12),
        method: method.clone(),
        path: path.clone(),
        query: query.clone(),
        function: function_name(&method, &path),
        ip: crate::api::request_ip(&headers),
        user_id: identity.user_id,
        username: identity.username,
        display_name: identity.display_name,
        user_role: identity.role,
        user_status: identity.status,
        email_verified: identity.email_verified,
        auth_kind: identity.auth_kind.to_string(),
        tokens: identity.tokens,
        user_agent: header_text(&headers, "user-agent"),
        request_headers: headers_to_json(&headers, !capture_bodies),
        request_content_type: request_content_type.clone(),
        request_capture: Mutex::new(BodyCapture::new(
            body_limit,
            request_content_type,
            Arc::clone(&capture_budget),
        )),
        response_capture: Mutex::new(BodyCapture::new(body_limit, String::new(), capture_budget)),
        response_status: AtomicU16::new(0),
        response_headers: Mutex::new("{}".to_string()),
        started_at: crate::util::now_iso(),
        started: Instant::now(),
        abort_tx: watch::channel(false).0,
        finished: AtomicBool::new(false),
    });
    monitor.register(Arc::clone(&connection));

    let body = if capture_bodies {
        capture_request_body(body, Arc::clone(&connection))
    } else {
        body
    };
    let request = Request::from_parts(parts, body);
    let handler = next.run(request);
    tokio::pin!(handler);
    let mut abort_rx = connection.abort_tx.subscribe();
    let response = tokio::select! {
        response = &mut handler => response,
        changed = abort_rx.changed() => {
            if changed.is_ok() && connection.is_aborted() {
                connection.complete(
                    &monitor,
                    Some(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
                    true,
                    Some("管理员强制断开连接".to_string()),
                );
                return aborted_response();
            }
            handler.as_mut().await
        }
    };
    if connection.is_aborted() {
        connection.complete(
            &monitor,
            Some(StatusCode::SERVICE_UNAVAILABLE.as_u16()),
            true,
            Some("管理员强制断开连接".to_string()),
        );
        return aborted_response();
    }

    let (parts, body) = response.into_parts();
    connection.set_response(parts.status, &parts.headers);
    let body = monitored_body(body, connection, monitor);
    Response::from_parts(parts, body)
}

fn function_name(method: &str, path: &str) -> String {
    if let Some(relative) = path
        .strip_prefix("/admin/api")
        .or_else(|| path.strip_prefix("/api"))
    {
        return admin_function(method, relative)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{method} {relative}"));
    }
    if let Some(relative) = path.strip_prefix("/account/api") {
        return account_function(method, relative)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{method} {relative}"));
    }
    if path.starts_with("/themes/") {
        return "下载主题包".to_string();
    }
    if path.starts_with("/previews/") {
        return "获取预览图".to_string();
    }
    if path.starts_with("/screenshots/") {
        return "获取截图".to_string();
    }
    if path.starts_with("/avatars/") {
        return "获取头像".to_string();
    }
    match path {
        "/submit" => "提交主题投稿".to_string(),
        "/catalog.json" => "获取主题目录".to_string(),
        "/health" | "/healthz" => "健康检查".to_string(),
        _ => format!("{method} {path}"),
    }
}

fn admin_function<'a>(method: &str, path: &str) -> Option<&'a str> {
    Some(match (method, path) {
        ("POST", "/login") => "管理员登录",
        ("POST", "/logout") => "管理员退出",
        ("GET", "/session") => "读取管理员会话",
        ("POST", "/setup") => "初始化管理员",
        ("GET", "/status") => "读取控制台状态",
        ("GET", "/stats") => "读取站点统计",
        ("GET", "/themes") => "读取主题列表",
        ("GET", "/pending") => "读取待审核列表",
        ("GET", "/authors") => "读取作者列表",
        ("GET", "/preview") => "预览主题",
        ("GET", "/storage") => "读取存储信息",
        ("GET", "/storage/job") => "读取存储任务",
        ("POST", "/upload-theme") => "上传主题",
        ("POST", "/password") => "修改管理员密码",
        ("POST", "/delete-theme") => "删除主题",
        ("POST", "/approve") => "批准主题投稿",
        ("POST", "/reject") => "拒绝主题投稿",
        ("POST", "/theme-tags") => "修改主题标签",
        ("POST", "/theme-meta") => "修改主题信息",
        ("GET", "/maintenance") => "读取维护状态",
        ("POST", "/maintenance") => "切换维护模式",
        ("POST", "/author/revoke") => "撤销作者许可",
        ("POST", "/author/delete") => "删除作者许可",
        ("POST", "/author/batch") => "批量处理作者",
        ("POST", "/author/restrictions") => "保存作者限制",
        ("POST", "/sync") => "同步主题目录",
        ("POST", "/test-catalog") => "测试主题目录",
        ("POST", "/storage/test") => "测试存储连接",
        ("POST", "/storage/root") => "修改存储目录",
        ("POST", "/storage/backend") => "切换存储后端",
        ("POST", "/storage/limit") => "修改存储配额",
        ("GET", "/users") => "读取用户列表",
        ("GET", "/author-requests") => "读取作者申请",
        ("GET", "/audit") => "读取操作日志",
        ("POST", "/username") => "修改管理员用户名",
        ("GET", "/theme-id/next") => "生成主题 ID",
        ("GET", "/follows") => "读取关注关系",
        ("GET", "/network") => "读取网络配置",
        ("POST", "/network") => "保存网络配置",
        ("POST", "/network/test") => "测试网络配置",
        ("GET", "/auth-settings") => "读取认证配置",
        ("POST", "/auth-settings") => "保存认证配置",
        ("POST", "/auth-settings/test-email") => "测试邮件发送",
        ("POST", "/auth-settings/test-github") => "测试 GitHub 登录",
        ("GET", "/storage/sources") => "读取存储来源",
        ("POST", "/storage/sources/save") => "保存存储来源",
        ("POST", "/storage/sources/delete") => "删除存储来源",
        ("POST", "/storage/sources/test") => "测试存储来源",
        ("POST", "/storage/sources/browse") => "浏览存储来源",
        ("POST", "/storage/sources/import") => "导入存储主题",
        ("GET", "/performance") => "读取性能配置",
        ("POST", "/performance") => "保存性能配置",
        ("GET", "/performance/stats") => "读取压缩统计",
        ("GET", "/performance/live") => "读取实时性能",
        ("POST", "/performance/stats/reset") => "重置性能统计",
        ("POST", "/performance/restart") => "重启管理服务",
        ("GET", "/comments") => "读取评论列表",
        ("POST", "/comments/delete") => "删除评论",
        ("POST", "/comments/clear") => "清空评论",
        ("GET", "/comments/settings") => "读取评论设置",
        ("POST", "/comments/settings") => "保存评论设置",
        ("POST", "/uploads/init") => "初始化上传任务",
        ("POST", "/tokens/create") => "创建访问令牌",
        ("POST", "/tokens/update") => "更新访问令牌",
        ("POST", "/tokens/delete") => "删除访问令牌",
        ("GET", "/tokens") => "读取访问令牌",
        ("POST", "/author-requests/{id}/approve") => "批准作者申请",
        ("POST", "/author-requests/{id}/reject") => "拒绝作者申请",
        _ if path.starts_with("/users/") => user_function(method, path)?,
        _ if path.starts_with("/author-requests/") && path.ends_with("/approve") => "批准作者申请",
        _ if path.starts_with("/author-requests/") && path.ends_with("/reject") => "拒绝作者申请",
        _ if path.starts_with("/uploads/") && method == "GET" => "读取上传任务",
        _ if path.starts_with("/uploads/") && method == "DELETE" => "取消上传任务",
        _ if path.starts_with("/uploads/") => "上传分片",
        _ => return None,
    })
}

fn user_function<'a>(method: &str, path: &str) -> Option<&'a str> {
    if method != "POST" && method != "GET" {
        return None;
    }
    if method == "GET" {
        return Some("读取用户详情");
    }
    Some(if path.ends_with("/approve") {
        "批准用户"
    } else if path.ends_with("/reject") {
        "拒绝用户"
    } else if path.ends_with("/disable") {
        "禁用账号"
    } else if path.ends_with("/enable") {
        "启用账号"
    } else if path.ends_with("/delete") {
        "删除账号"
    } else if path.ends_with("/reset-password") {
        "重置用户密码"
    } else if path.ends_with("/role") {
        "修改用户角色"
    } else if path.ends_with("/author/bind") {
        "绑定作者"
    } else if path.ends_with("/author/reset") {
        "重置作者 ID"
    } else {
        return None;
    })
}

fn account_function<'a>(method: &str, path: &str) -> Option<&'a str> {
    Some(match (method, path) {
        ("GET", "/config") => "读取用户中心配置",
        ("POST", "/register") => "注册账号",
        ("POST", "/auth/login") => "用户登录",
        ("POST", "/auth/logout") => "用户退出",
        ("GET", "/session") => "读取用户会话",
        ("GET", "/auth/verify-email") => "验证邮箱",
        ("POST", "/auth/resend-verification") => "重发验证邮件",
        ("POST", "/auth/forgot-password") => "发送重置邮件",
        ("POST", "/auth/reset-password") => "重置密码",
        ("GET", "/auth/github/start") => "GitHub 登录",
        ("GET", "/auth/github/callback") => "GitHub 登录回调",
        ("POST", "/auth/github/unlink") => "解绑 GitHub",
        ("POST", "/auth/device/start") => "发起设备登录",
        ("GET", "/auth/device/poll") => "轮询设备登录",
        ("GET", "/auth/device/info") => "读取设备登录信息",
        ("POST", "/auth/device/approve") => "确认设备登录",
        ("GET", "/follows") => "读取我的关注",
        ("POST", "/follows/follow") => "关注作者",
        ("POST", "/follows/unfollow") => "取消关注",
        ("GET", "/follows/leaderboard") => "读取关注排行榜",
        ("GET", "/preferences") => "读取偏好设置",
        ("POST", "/preferences") => "保存偏好设置",
        ("GET", "/profile") => "读取个人资料",
        ("POST", "/profile") => "保存个人资料",
        ("POST", "/password") => "修改用户密码",
        ("POST", "/email") => "修改邮箱",
        ("POST", "/avatar") => "上传头像",
        ("GET", "/authors") => "读取我的作者",
        ("GET", "/author/requests") => "读取作者申请",
        ("POST", "/author/requests") => "提交作者申请",
        ("GET", "/discover") => "主题广场推荐",
        ("GET", "/history") => "读取最近主题",
        ("POST", "/history/view") => "记录主题查看",
        ("POST", "/history/delete") => "删除主题历史",
        ("GET", "/messages") => "读取消息",
        ("GET", "/users/search") => "搜索用户",
        ("POST", "/comments/delete") => "删除我的留言",
        ("POST", "/comments/clear") => "清空我的留言",
        ("GET", "/my-themes") => "读取我的主题",
        ("POST", "/my-themes/withdraw") => "撤回我的主题",
        ("POST", "/my-themes/restore") => "恢复我的主题",
        ("POST", "/my-themes/delete") => "删除我的主题",
        ("GET", "/submissions") => "读取投稿记录",
        ("POST", "/submissions/withdraw") => "撤回投稿",
        ("POST", "/submissions/delete") => "删除投稿记录",
        ("GET", "/preview") => "预览主题",
        ("GET", "/theme-id/next") => "生成主题 ID",
        ("POST", "/submit") => "提交主题",
        _ if path.starts_with("/users/") && path.ends_with("/avatar") => "获取用户头像",
        _ if path.starts_with("/author/requests/") && path.ends_with("/generate") => "生成作者 ID",
        _ if path.starts_with("/themes/") && path.ends_with("/comments") => {
            if method == "POST" {
                "发表评论"
            } else {
                "读取主题评论"
            }
        }
        _ if path.starts_with("/themes/") && path.ends_with("/download") => "下载主题",
        _ if path.starts_with("/my-themes/") && path.ends_with("/download") => "下载我的主题",
        _ => return None,
    })
}

#[derive(Debug, Default, Clone, Deserialize)]
struct LogQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default, rename = "ipFamily")]
    ip_family: Option<String>,
    #[serde(default, rename = "excludeIp")]
    exclude_ip: Option<String>,
    #[serde(default, rename = "excludeUser")]
    exclude_user: Option<String>,
    #[serde(default, rename = "watchIp")]
    watch_ip: Option<String>,
    #[serde(default, rename = "watchUser")]
    watch_user: Option<String>,
    #[serde(default, rename = "watchOnly")]
    watch_only: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

fn filter_terms(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .take(50)
        .map(|item| item.chars().take(128).collect())
        .collect()
}

fn log_filters(query: &LogQuery) -> (String, Vec<SqlValue>) {
    let mut where_sql = String::from(" WHERE 1 = 1");
    let mut values = Vec::new();
    if let Some(q) = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        where_sql.push_str(
            " AND (path LIKE ? OR function LIKE ? OR query LIKE ? OR username LIKE ? OR display_name LIKE ? OR ip LIKE ?)",
        );
        let like = format!("%{q}%");
        for _ in 0..6 {
            values.push(SqlValue::Text(like.clone()));
        }
    }
    if let Some(user) = query
        .user
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        where_sql.push_str(" AND (username LIKE ? OR display_name LIKE ?)");
        let like = format!("%{user}%");
        values.push(SqlValue::Text(like.clone()));
        values.push(SqlValue::Text(like));
    }
    if let Some(ip) = query
        .ip
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        where_sql.push_str(" AND ip LIKE ?");
        values.push(SqlValue::Text(format!("%{ip}%")));
    }
    if let Some(auth) = query
        .auth
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        where_sql.push_str(" AND auth_kind = ?");
        values.push(SqlValue::Text(auth.to_string()));
    }
    if let Some(method) = query
        .method
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        where_sql.push_str(" AND method = ?");
        values.push(SqlValue::Text(method.to_ascii_uppercase()));
    }
    if let Some(status) = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match status {
            "aborted" => where_sql.push_str(" AND aborted = 1"),
            "2xx" => where_sql.push_str(" AND status >= 200 AND status < 300"),
            "3xx" => where_sql.push_str(" AND status >= 300 AND status < 400"),
            "4xx" => where_sql.push_str(" AND status >= 400 AND status < 500"),
            "5xx" => where_sql.push_str(" AND status >= 500 AND status < 600"),
            other => {
                if let Ok(value) = other.parse::<i64>() {
                    where_sql.push_str(" AND status = ?");
                    values.push(SqlValue::Integer(value));
                }
            }
        }
    }
    if let Some(family) = query
        .ip_family
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|value| !value.is_empty())
    {
        match family.as_str() {
            "ipv4" => where_sql.push_str(" AND instr(ip, ':') = 0"),
            "ipv6" => where_sql.push_str(" AND instr(ip, ':') > 0"),
            _ => {}
        }
    }
    for term in filter_terms(query.exclude_ip.as_deref()) {
        if term.contains('*') {
            where_sql.push_str(" AND ip NOT LIKE ?");
            values.push(SqlValue::Text(term.replace('*', "%")));
        } else {
            where_sql.push_str(" AND ip != ?");
            values.push(SqlValue::Text(term));
        }
    }
    for term in filter_terms(query.exclude_user.as_deref()) {
        where_sql.push_str(
            " AND LOWER(COALESCE(username, '')) NOT LIKE ? AND LOWER(COALESCE(display_name, '')) NOT LIKE ?",
        );
        let like = format!("%{}%", term.to_lowercase());
        values.push(SqlValue::Text(like.clone()));
        values.push(SqlValue::Text(like));
    }
    if query
        .watch_only
        .as_deref()
        .is_some_and(|value| parse_bool(value, false))
    {
        let mut watch_clauses = Vec::new();
        let mut watch_values = Vec::new();
        for term in filter_terms(query.watch_ip.as_deref()) {
            if term.contains('*') {
                watch_clauses.push("ip LIKE ?");
                watch_values.push(SqlValue::Text(term.replace('*', "%")));
            } else {
                watch_clauses.push("ip = ?");
                watch_values.push(SqlValue::Text(term));
            }
        }
        for term in filter_terms(query.watch_user.as_deref()) {
            watch_clauses.push("LOWER(COALESCE(username, '')) LIKE ?");
            watch_clauses.push("LOWER(COALESCE(display_name, '')) LIKE ?");
            let like = format!("%{}%", term.to_lowercase());
            watch_values.push(SqlValue::Text(like.clone()));
            watch_values.push(SqlValue::Text(like));
        }
        if watch_clauses.is_empty() {
            where_sql.push_str(" AND 0 = 1");
        } else {
            where_sql.push_str(" AND (");
            where_sql.push_str(&watch_clauses.join(" OR "));
            where_sql.push(')');
            values.extend(watch_values);
        }
    }
    (where_sql, values)
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "connectionId": row.get::<_, String>(1)?,
        "method": row.get::<_, String>(2)?,
        "path": row.get::<_, String>(3)?,
        "query": row.get::<_, String>(4)?,
        "function": row.get::<_, String>(5)?,
        "status": row.get::<_, i64>(6)?,
        "userId": row.get::<_, Option<String>>(7)?,
        "username": row.get::<_, Option<String>>(8)?,
        "displayName": row.get::<_, Option<String>>(9)?,
        "authKind": row.get::<_, String>(10)?,
        "ip": row.get::<_, String>(11)?,
        "requestBytes": row.get::<_, i64>(12)?,
        "responseBytes": row.get::<_, i64>(13)?,
        "startedAt": row.get::<_, String>(14)?,
        "endedAt": row.get::<_, String>(15)?,
        "durationMs": row.get::<_, i64>(16)?,
        "aborted": row.get::<_, i64>(17)? != 0,
        "error": row.get::<_, Option<String>>(18)?,
    }))
}

fn full_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "connectionId": row.get::<_, String>(1)?,
        "method": row.get::<_, String>(2)?,
        "path": row.get::<_, String>(3)?,
        "query": row.get::<_, String>(4)?,
        "function": row.get::<_, String>(5)?,
        "status": row.get::<_, i64>(6)?,
        "userId": row.get::<_, Option<String>>(7)?,
        "username": row.get::<_, Option<String>>(8)?,
        "displayName": row.get::<_, Option<String>>(9)?,
        "authKind": row.get::<_, String>(10)?,
        "ip": row.get::<_, String>(11)?,
        "userAgent": row.get::<_, String>(12)?,
        "requestHeaders": parse_json_text(row.get::<_, Option<String>>(13)?),
        "requestContentType": row.get::<_, String>(14)?,
        "requestBody": parse_json_text(row.get::<_, Option<String>>(15)?),
        "requestBytes": row.get::<_, i64>(16)?,
        "requestTruncated": row.get::<_, i64>(17)? != 0,
        "responseHeaders": parse_json_text(row.get::<_, Option<String>>(18)?),
        "responseContentType": row.get::<_, String>(19)?,
        "responseBody": parse_json_text(row.get::<_, Option<String>>(20)?),
        "responseBytes": row.get::<_, i64>(21)?,
        "responseTruncated": row.get::<_, i64>(22)? != 0,
        "startedAt": row.get::<_, String>(23)?,
        "endedAt": row.get::<_, String>(24)?,
        "durationMs": row.get::<_, i64>(25)?,
        "aborted": row.get::<_, i64>(26)? != 0,
        "error": row.get::<_, Option<String>>(27)?,
        "userRole": row.get::<_, Option<String>>(28)?,
        "userStatus": row.get::<_, Option<String>>(29)?,
        "emailVerified": row.get::<_, Option<i64>>(30)?.map(|value| value != 0),
    }))
}

async fn fetch_summaries(
    db: &Database,
    query: &LogQuery,
    limit: i64,
    offset: i64,
) -> anyhow::Result<(Vec<Value>, i64)> {
    let (where_sql, values) = log_filters(query);
    let count_values = values.clone();
    db.call(move |conn| {
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM audit.api_request_logs{where_sql}"),
            rusqlite::params_from_iter(count_values.iter()),
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM audit.api_request_logs{where_sql}
             ORDER BY id DESC LIMIT ? OFFSET ?"
        ))?;
        let mut params = values;
        params.push(SqlValue::Integer(limit));
        params.push(SqlValue::Integer(offset));
        let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), summary_row)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((items, total))
    })
    .await
}

async fn fetch_log(db: &Database, id: i64) -> anyhow::Result<Option<Value>> {
    db.call(move |conn| {
        let mut statement =
            conn.prepare(&format!("SELECT {FULL_COLUMNS} FROM audit.api_request_logs WHERE id = ?1"))?;
        let mut rows = statement.query([id])?;
        Ok(rows.next()?.map(full_row).transpose()?)
    })
    .await
}

async fn fetch_log_by_connection(
    db: &Database,
    connection_id: String,
) -> anyhow::Result<Option<Value>> {
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {FULL_COLUMNS} FROM audit.api_request_logs
             WHERE connection_id = ?1 ORDER BY id DESC LIMIT 1"
        ))?;
        let mut rows = statement.query([connection_id])?;
        Ok(rows.next()?.map(full_row).transpose()?)
    })
    .await
}

async fn fetch_full_batch(
    db: &Database,
    query: &LogQuery,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<Value>> {
    let (where_sql, values) = log_filters(query);
    db.call(move |conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT {FULL_COLUMNS} FROM audit.api_request_logs{where_sql}
             ORDER BY id DESC LIMIT ? OFFSET ?"
        ))?;
        let mut params = values;
        params.push(SqlValue::Integer(limit));
        params.push(SqlValue::Integer(offset));
        let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), full_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

async fn count_logs(db: &Database, query: &LogQuery) -> anyhow::Result<i64> {
    let (where_sql, values) = log_filters(query);
    db.call(move |conn| {
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM audit.api_request_logs{where_sql}"),
            rusqlite::params_from_iter(values.iter()),
            |row| row.get(0),
        )?;
        Ok(total)
    })
    .await
}

async fn delete_logs(db: &Database, ids: &[i64]) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let ids = ids.to_vec();
    db.call(move |conn| {
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let deleted = conn.execute(
            &format!("DELETE FROM audit.api_request_logs WHERE id IN ({placeholders})"),
            rusqlite::params_from_iter(ids.iter()),
        )?;
        Ok(deleted)
    })
    .await
}

async fn clear_logs(db: &Database) -> anyhow::Result<usize> {
    db.call(move |conn| {
        let deleted = conn.execute("DELETE FROM audit.api_request_logs", [])?;
        Ok(deleted)
    })
    .await
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/monitor/live", get(live))
        .route("/monitor/active/{id}", get(active_detail))
        .route("/monitor/active/{id}/abort", post(abort_connection))
        .route("/monitor/logs", get(list_logs))
        .route("/monitor/logs/export", get(export_logs))
        .route("/monitor/logs/batch-delete", post(delete_logs_handler))
        .route("/monitor/logs/clear", post(clear_logs_handler))
        .route(
            "/monitor/logs/connection/{id}",
            get(log_detail_by_connection),
        )
        .route("/monitor/logs/{id}", get(log_detail))
}

async fn live(State(state): State<AppState>) -> Json<Value> {
    let monitor = &state.monitor;
    let last_24h = monitor.last_24h().await;
    let mut stats = monitor.stats_snapshot();
    if let Value::Object(map) = &mut stats {
        map.insert("last24h".into(), json!(last_24h));
    }
    Json(json!({
        "ok": true,
        "active": monitor.active_list(),
        "recent": monitor.recent_list(),
        "stats": stats,
    }))
}

async fn active_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(connection) = state.monitor.active_connection(&id) else {
        return Err(ApiError::not_found("连接已结束或不存在"));
    };
    Ok(Json(json!({
        "ok": true,
        "connection": connection.detail_value(),
    })))
}

async fn abort_connection(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(connection) = state.monitor.abort(&id) else {
        return Err(ApiError::not_found("连接已结束或不存在"));
    };
    for token in &connection.tokens {
        state.sessions.remove(token).await;
    }
    let detail = format!(
        "path={} method={} user={} ip={} sessions={}",
        connection.path,
        connection.method,
        connection
            .username
            .clone()
            .or_else(|| connection.display_name.clone())
            .unwrap_or_else(|| "-".into()),
        connection.ip,
        connection.tokens.len(),
    );
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "api.abort",
        Some("api_connection"),
        Some(&id),
        Some(&detail),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "aborted": 1,
        "id": id,
        "sessionsRevoked": connection.tokens.len(),
    })))
}

async fn list_logs(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    let page = query.page.unwrap_or(0).max(0);
    let limit = query.limit.unwrap_or(25).clamp(1, 200);
    let offset = page.saturating_mul(limit);
    let (items, total) = fetch_summaries(&state.db, &query, limit, offset)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "items": items,
        "total": total,
        "page": page,
        "limit": limit,
        "stats": state.monitor.stats_snapshot(),
    })))
}

async fn log_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = id
        .parse::<i64>()
        .map_err(|_| ApiError::bad("无效的日志 ID"))?;
    let Some(log) = fetch_log(&state.db, id).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found("日志不存在或已被删除"));
    };
    Ok(Json(json!({"ok": true, "log": log})))
}

async fn log_detail_by_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(log) = fetch_log_by_connection(&state.db, connection_id)
        .await
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::not_found("日志尚未写入或已被删除"));
    };
    Ok(Json(json!({"ok": true, "log": log})))
}

#[derive(Debug, Deserialize)]
struct DeleteLogsBody {
    #[serde(default)]
    ids: Vec<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct ClearLogsBody {}

async fn delete_logs_handler(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Json(body): Json<DeleteLogsBody>,
) -> ApiResult<Json<Value>> {
    let ids = body
        .ids
        .into_iter()
        .filter(|id| *id > 0)
        .take(MAX_DELETE_IDS)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(ApiError::bad("请选择要删除的日志"));
    }
    let deleted = delete_logs(&state.db, &ids)
        .await
        .map_err(ApiError::internal)?;
    state.monitor.refresh_counts().await;
    let id_list = ids
        .iter()
        .take(20)
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "api.logs.delete",
        Some("api_log"),
        None,
        Some(&format!("count={deleted} ids={id_list}")),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "deleted": deleted})))
}

async fn clear_logs_handler(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Json(_body): Json<ClearLogsBody>,
) -> ApiResult<Json<Value>> {
    let deleted = clear_logs(&state.db).await.map_err(ApiError::internal)?;
    state.monitor.refresh_counts().await;
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "api.logs.clear",
        Some("api_log"),
        None,
        Some(&format!("count={deleted}")),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "deleted": deleted})))
}

async fn export_logs(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Response> {
    let total = count_logs(&state.db, &query)
        .await
        .map_err(ApiError::internal)?;
    let detail = format!(
        "count={total} q={} user={} ip={} auth={} status={} ipFamily={} excludeIp={} excludeUser={}",
        query.q.clone().unwrap_or_default(),
        query.user.clone().unwrap_or_default(),
        query.ip.clone().unwrap_or_default(),
        query.auth.clone().unwrap_or_default(),
        query.status.clone().unwrap_or_default(),
        query.ip_family.clone().unwrap_or_default(),
        query.exclude_ip.clone().unwrap_or_default(),
        query.exclude_user.clone().unwrap_or_default(),
    );
    crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "api.logs.export",
        Some("api_log"),
        None,
        Some(&detail),
    )
    .await
    .map_err(ApiError::internal)?;

    let (tx, rx) = mpsc::channel::<String>(8);
    let db = state.db.clone();
    let filter = query;
    tokio::spawn(async move {
        let mut offset = 0;
        loop {
            match fetch_full_batch(&db, &filter, MAX_EXPORT_BATCH, offset).await {
                Ok(rows) if rows.is_empty() => break,
                Ok(rows) => {
                    let count = rows.len() as i64;
                    for row in rows {
                        let line = serde_json::to_string(&row).unwrap_or_else(|_| "{}".into());
                        if tx.send(format!("{line}\n")).await.is_err() {
                            return;
                        }
                    }
                    if count < MAX_EXPORT_BATCH {
                        break;
                    }
                    offset += count;
                }
                Err(error) => {
                    let line = json!({"error": error.to_string()}).to_string();
                    let _ = tx.send(format!("{line}\n")).await;
                    break;
                }
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|line| (Ok::<Bytes, std::io::Error>(Bytes::from(line)), rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson; charset=utf-8")
        .header(
            "Content-Disposition",
            format!(
                "attachment; filename=\"sukisu-api-logs-{}.ndjson\"",
                crate::util::now_compact()
            ),
        )
        .header("Cache-Control", "no-store")
        .header("X-Total-Count", total.to_string())
        .body(Body::from_stream(stream))
        .map_err(ApiError::internal)
}
