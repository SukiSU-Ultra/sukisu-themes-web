use std::net::{IpAddr, TcpListener};
use std::path::{Path, PathBuf};

use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::SessionInfo;
use crate::api::{json_body, ApiError, AppState};
use crate::config::{self, NetworkSettings};
use crate::system;
use crate::util::random_hex;

type ApiResult<T> = Result<T, ApiError>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/network", get(network_get).post(network_apply))
        .route("/network/test", post(network_test))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkBody {
    bind: String,
    port: u16,
    #[serde(default)]
    ip_family: Option<crate::net::IpFamily>,
    #[serde(default)]
    outbound_ip_family: Option<crate::net::IpFamily>,
    #[serde(default)]
    domains: Vec<String>,
    #[serde(default)]
    domains_text: String,
    public_url: String,
    admin_url: String,
    account_url: String,
    github_callback: String,
}

impl NetworkBody {
    fn domains(&self) -> Vec<String> {
        if !self.domains.is_empty() {
            return self.domains.clone();
        }
        self.domains_text
            .split([',', '\n'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn validate(&self) -> anyhow::Result<NetworkSettings> {
        config::validate_network(
            &self.bind,
            self.port,
            self.ip_family.unwrap_or_default(),
            self.outbound_ip_family.unwrap_or_default(),
            &self.domains(),
            &self.public_url,
            &self.admin_url,
            &self.account_url,
            &self.github_callback,
        )
    }
}

fn site_config() -> Option<crate::site::SiteConfig> {
    let file = std::env::var_os("SUKISU_SITE_CONFIG")?;
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn site_public_entry() -> Option<String> {
    let config = site_config()?;
    config
        .peers
        .iter()
        .find(|peer| peer.id == config.node_id)
        .map(|peer| peer.base_url.clone())
}

fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    authority.split(':').next().unwrap_or("")
}

/// Loopback and link-local addresses only work on this machine or link.
/// Private LAN addresses are still valid endpoints for LAN users, so they are
/// kept; loopback is only the internal listener.
fn is_direct_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xffc0) == 0xfe80)
        }
        Err(_) => true,
    }
}

/// The address users actually connect to. `bind:port` is only the local
/// listener and may be a random port in complete-site mode.
fn public_entry(settings: &NetworkSettings) -> (String, &'static str) {
    let public_url = settings.public_url.trim();
    if !public_url.is_empty() && is_direct_host(url_host(public_url)) {
        return (public_url.trim_end_matches('/').to_string(), "network");
    }
    if let Some(url) = site_public_entry() {
        if is_direct_host(url_host(&url)) {
            return (url.trim_end_matches('/').to_string(), "site");
        }
    }
    if let Some(domain) = settings.domains.first().map(|value| value.trim()) {
        let domain = domain.trim_end_matches('/');
        if !domain.is_empty() {
            let url = if domain.contains("://") {
                domain.to_string()
            } else {
                format!("https://{domain}")
            };
            if is_direct_host(url_host(&url)) {
                return (url, "domain");
            }
        }
    }
    (String::new(), "none")
}

/// In a complete site, the reverse proxy points at the supervisor's bind
/// address. The child process listens on a separately allocated random port.
fn proxy_target(settings: &NetworkSettings) -> String {
    if let Some(config) = site_config() {
        config.bind.to_string()
    } else {
        settings.upstream()
    }
}

/// Last resort: the address this admin page was reached on. Behind a reverse
/// proxy the forwarded headers carry the public scheme and host.
fn request_entry(headers: &HeaderMap) -> Option<String> {
    let first = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let host = first(header::HeaderName::from_static("x-forwarded-host"))
        .or_else(|| first(header::HOST))?;
    if !is_direct_host(url_host(&host)) {
        return None;
    }
    let scheme = first(header::HeaderName::from_static("x-forwarded-proto"))
        .unwrap_or_else(|| "http".to_string());
    Some(format!("{}://{}", scheme.trim_end_matches(':'), host))
}

fn health_host(settings: &NetworkSettings) -> String {
    match settings.bind.as_str() {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        value if value.contains(':') => format!("[{value}]"),
        value => value.to_string(),
    }
}

fn test_bind(settings: &NetworkSettings) -> ApiResult<()> {
    let ip: IpAddr = settings
        .bind
        .parse()
        .map_err(|_| ApiError::bad("监听地址无效"))?;
    TcpListener::bind((ip, settings.port))
        .map(|_| ())
        .map_err(|error| {
            ApiError::bad(format!(
                "无法监听 {}:{}：{error}",
                settings.bind, settings.port
            ))
        })
}

fn copy_if_exists(from: &Path, to: &Path) -> anyhow::Result<()> {
    if from.is_file() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

fn make_backup(paths: &config::Paths) -> anyhow::Result<PathBuf> {
    let parent = paths.network_env.parent().unwrap_or_else(|| Path::new("."));
    let directory = parent.join(format!("network-backup-{}", random_hex(4)));
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
    }
    copy_if_exists(&paths.network_env, &directory.join("network.env"))?;
    copy_if_exists(&paths.admin_config, &directory.join("config.env"))?;
    copy_if_exists(&paths.account_env, &directory.join("account.env"))?;
    Ok(directory)
}

fn restore_backup(paths: &config::Paths, backup: &Path) {
    for (name, target) in [
        ("network.env", &paths.network_env),
        ("config.env", &paths.admin_config),
        ("account.env", &paths.account_env),
    ] {
        let source = backup.join(name);
        if source.is_file() {
            let _ = std::fs::copy(source, target);
        }
    }
}

async fn network_get(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let settings = config::read_network_settings(&state.paths);
    let (public_entry, public_entry_source) = public_entry(&settings);
    let (public_entry, public_entry_source) = if public_entry_source == "none" {
        match request_entry(&headers) {
            Some(entry) => (entry, "request"),
            None => (public_entry, public_entry_source),
        }
    } else {
        (public_entry, public_entry_source)
    };
    let upstream = proxy_target(&settings);
    let internal = settings.upstream();
    Json(json!({
        "ok": true,
        "settings": settings,
        "upstream": upstream,
        "internal": internal,
        "mode": if system::site_managed() { "site" } else { "standalone" },
        "publicEntry": public_entry,
        "publicEntrySource": public_entry_source,
        "systemctlDisabled": system::systemctl_disabled(),
        "adminService": system::service_state(system::service_unit()).await,
        "serviceUnit": system::service_unit(),
    }))
}

async fn network_test(State(state): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: NetworkBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("网络配置格式错误：{error}")))?;
    let settings = body
        .validate()
        .map_err(|error| ApiError::bad(error.to_string()))?;
    let current = config::read_network_settings(&state.paths);
    let site_bind = site_config().map(|config| config.bind);
    if site_bind.is_some() {
        let ip: IpAddr = settings
            .bind
            .parse()
            .map_err(|_| ApiError::bad("监听地址无效"))?;
        if !ip.is_loopback() {
            return Err(ApiError::bad("完整站点只允许回环监听地址"));
        }
    }
    if settings.port != current.port && site_bind.map(|bind| bind.port()) != Some(settings.port) {
        test_bind(&settings)?;
    }
    Ok(Json(json!({
        "ok": true,
        "message": "监听地址可用，URL 格式有效",
        "settings": settings,
    })))
}

async fn network_apply(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: NetworkBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("网络配置格式错误：{error}")))?;
    let settings = body
        .validate()
        .map_err(|error| ApiError::bad(error.to_string()))?;
    let current = config::read_network_settings(&state.paths);
    let site = site_config();
    let bind_ip: IpAddr = settings
        .bind
        .parse()
        .map_err(|_| ApiError::bad("监听地址无效"))?;
    if site.is_some() && !bind_ip.is_loopback() {
        return Err(ApiError::bad("完整站点只允许回环监听地址"));
    }
    let site_port = site.as_ref().map(|config| config.bind.port());
    if settings.port != current.port && site_port != Some(settings.port) {
        test_bind(&settings)?;
    }

    let backup = make_backup(&state.paths).map_err(ApiError::internal)?;
    let mut persisted = settings.clone();
    if site.is_some() {
        // The site supervisor owns the child's port and loopback family; keep
        // them aligned with the new site bind instead of the public target.
        persisted.bind = (if bind_ip.is_ipv4() { "127.0.0.1" } else { "::1" }).to_string();
        persisted.port = current.port;
    }
    if let Err(error) = config::write_network_settings(&state.paths, &persisted) {
        restore_backup(&state.paths, &backup);
        return Err(ApiError::internal(error));
    }

    let mut site_changed = false;
    if let Some(config) = &site {
        let next = std::net::SocketAddr::new(bind_ip, settings.port);
        if next != config.bind {
            if let Err(error) = crate::site::update_bind(next) {
                restore_backup(&state.paths, &backup);
                return Err(ApiError::internal(error));
            }
            site_changed = true;
        }
    }

    accounts_audit(&state, &actor.user_id, &settings).await;

    if site.is_some() {
        return Ok(Json(json!({
            "ok": true,
            "restart": false,
            "message": if site_changed {
                "网络配置已保存；站点监听地址已更新，请重启 sukisu-theme-site.service 生效"
            } else {
                "网络配置已保存；请重启 sukisu-theme-site.service 使配置生效"
            },
            "settings": settings,
        })));
    }

    if system::systemctl_disabled() {
        return Ok(Json(json!({
            "ok": true,
            "restart": false,
            "message": "配置已保存；当前环境禁用了 systemctl，请手动重启服务",
            "settings": settings,
        })));
    }

    let script = std::env::var("THEME_NETWORK_SCRIPT")
        .unwrap_or_else(|_| "/opt/sukisu-theme-web/apply-network.sh".to_string());
    let backup_text = backup.to_string_lossy().to_string();
    if let Err(error) = system::schedule_network_apply(
        &script,
        settings.port,
        current.port,
        &backup_text,
        &health_host(&settings),
    ) {
        restore_backup(&state.paths, &backup);
        return Err(ApiError::internal(error));
    }

    Ok(Json(json!({
        "ok": true,
        "restart": true,
        "message": "网络配置已保存，服务将在 2 秒后重启；失败会自动回滚",
        "settings": settings,
    })))
}

async fn accounts_audit(state: &AppState, actor: &str, settings: &NetworkSettings) {
    let detail = format!(
        "{}:{} listen={} outbound={} domains={} public={}",
        settings.bind,
        settings.port,
        settings.ip_family.as_str(),
        settings.outbound_ip_family.as_str(),
        settings.domains.join(","),
        settings.public_url
    );
    let _ = crate::accounts::audit(
        &state.db,
        Some(actor),
        "network.update",
        Some("network"),
        None,
        Some(&detail),
    )
    .await;
}
