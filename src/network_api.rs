use std::net::{IpAddr, TcpListener};
use std::path::{Path, PathBuf};

use axum::extract::{Extension, State};
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

async fn network_get(State(state): State<AppState>) -> Json<Value> {
    let settings = config::read_network_settings(&state.paths);
    Json(json!({
        "ok": true,
        "settings": settings,
        "upstream": settings.upstream(),
        "systemctlDisabled": system::systemctl_disabled(),
        "adminService": system::service_state("sukisu-theme-web.service").await,
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
    if settings.port != current.port {
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
    if settings.port != current.port {
        test_bind(&settings)?;
    }

    let backup = make_backup(&state.paths).map_err(ApiError::internal)?;
    if let Err(error) = config::write_network_settings(&state.paths, &settings) {
        restore_backup(&state.paths, &backup);
        return Err(ApiError::internal(error));
    }

    accounts_audit(&state, &actor.user_id, &settings).await;

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
