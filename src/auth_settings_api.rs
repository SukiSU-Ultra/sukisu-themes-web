use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::accounts::SessionInfo;
use crate::api::{json_body, ApiError, AppState};
use crate::config::{read_env, remove_env_key, set_env_key};
use crate::mailer;
use crate::oauth;

type ApiResult<T> = Result<T, ApiError>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/auth-settings",
            get(auth_settings_get).post(auth_settings_save),
        )
        .route("/auth-settings/test-email", post(test_email))
        .route("/auth-settings/test-github", post(test_github))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AuthSettingsBody {
    #[serde(default)]
    smtp_host: String,
    #[serde(default)]
    smtp_port: Option<u16>,
    #[serde(default)]
    smtp_username: String,
    #[serde(default)]
    smtp_password: String,
    #[serde(default)]
    smtp_from: String,
    #[serde(default)]
    smtp_tls: String,
    #[serde(default)]
    github_client_id: String,
    #[serde(default)]
    github_client_secret: String,
    #[serde(default)]
    github_callback: String,
    #[serde(default)]
    qq_app_id: String,
    #[serde(default)]
    qq_app_key: String,
    #[serde(default)]
    qq_callback: String,
    /// Optional because older clients do not send it; the current admin UI always does.
    #[serde(default)]
    session_ttl_days: Option<u64>,
    #[serde(default)]
    clear_smtp_password: bool,
    #[serde(default)]
    clear_github_secret: bool,
    #[serde(default)]
    clear_qq_key: bool,
}

#[derive(Deserialize)]
struct TestEmailBody {
    to: String,
}

fn state_value(state: &AppState) -> Value {
    let env = read_env(&state.paths.account_env);
    let smtp_host = env.get("SMTP_HOST").cloned().unwrap_or_default();
    let smtp_from = env.get("SMTP_FROM").cloned().unwrap_or_default();
    let github_id = env.get("GITHUB_CLIENT_ID").cloned().unwrap_or_default();
    let callback = env.get("GITHUB_CALLBACK_URL").cloned().unwrap_or_else(|| {
        format!(
            "{}api/auth/github/callback",
            mailer::account_base_url(&state.paths)
        )
    });
    let qq_app_id = env.get("QQ_APP_ID").cloned().unwrap_or_default();
    let qq_callback = env.get("QQ_CALLBACK_URL").cloned().unwrap_or_else(|| {
        format!(
            "{}api/auth/qq/callback",
            mailer::account_base_url(&state.paths)
        )
    });
    let session_ttl_secs = crate::config::web_session_ttl_secs(&state.paths);
    json!({
        "ok": true,
        "smtpHost": smtp_host,
        "smtpPort": env.get("SMTP_PORT").and_then(|value| value.parse::<u16>().ok()).unwrap_or(587),
        "smtpUsername": env.get("SMTP_USERNAME").cloned().unwrap_or_default(),
        "smtpFrom": smtp_from,
        "smtpTls": env.get("SMTP_TLS").cloned().unwrap_or_else(|| "starttls".to_string()),
        "smtpPasswordConfigured": env.get("SMTP_PASSWORD").map(|value| !value.is_empty()).unwrap_or(false),
        "smtpConfigured": !smtp_host.is_empty() && !smtp_from.is_empty(),
        "githubClientId": github_id,
        "githubClientSecretConfigured": env.get("GITHUB_CLIENT_SECRET").map(|value| !value.is_empty()).unwrap_or(false),
        "githubCallback": callback,
        "githubConfigured": !github_id.is_empty()
            && env.get("GITHUB_CLIENT_SECRET").map(|value| !value.is_empty()).unwrap_or(false),
        "qqAppId": qq_app_id,
        "qqAppKeyConfigured": env.get("QQ_APP_KEY").map(|value| !value.is_empty()).unwrap_or(false),
        "qqCallback": qq_callback,
        "qqConfigured": !qq_app_id.is_empty()
            && env.get("QQ_APP_KEY").map(|value| !value.is_empty()).unwrap_or(false),
        "sessionTtlDays": (session_ttl_secs / (24 * 60 * 60)).max(1),
        "sessionTtlSeconds": session_ttl_secs,
        "devMode": std::env::var("THEME_DEV_MODE").as_deref() == Ok("1"),
        "accountUrl": mailer::account_base_url(&state.paths),
    })
}

async fn auth_settings_get(State(state): State<AppState>) -> Json<Value> {
    Json(state_value(&state))
}

fn valid_callback(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}

async fn auth_settings_save(
    State(state): State<AppState>,
    Extension(actor): Extension<SessionInfo>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: AuthSettingsBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("配置格式错误：{error}")))?;
    let smtp_host = body.smtp_host.trim().to_string();
    let smtp_from = body.smtp_from.trim().to_string();
    let smtp_tls = body.smtp_tls.trim().to_ascii_lowercase();
    let smtp_tls = if smtp_tls.is_empty() {
        "starttls".to_string()
    } else {
        smtp_tls
    };
    if !matches!(smtp_tls.as_str(), "starttls" | "tls" | "none") {
        return Err(ApiError::bad("SMTP_TLS 只能是 starttls、tls 或 none"));
    }
    let smtp_port = body.smtp_port.unwrap_or(587);
    if smtp_host.is_empty() {
        for key in [
            "SMTP_HOST",
            "SMTP_PORT",
            "SMTP_USERNAME",
            "SMTP_PASSWORD",
            "SMTP_FROM",
            "SMTP_TLS",
        ] {
            let _ = remove_env_key(&state.paths.account_env, key);
        }
    } else {
        if smtp_port == 0 {
            return Err(ApiError::bad("SMTP 端口无效"));
        }
        if smtp_from.is_empty() || !smtp_from.contains('@') {
            return Err(ApiError::bad("请填写有效的发件人地址"));
        }
        set_env_key(&state.paths.account_env, "SMTP_HOST", &smtp_host)
            .map_err(ApiError::internal)?;
        set_env_key(
            &state.paths.account_env,
            "SMTP_PORT",
            &smtp_port.to_string(),
        )
        .map_err(ApiError::internal)?;
        set_env_key(
            &state.paths.account_env,
            "SMTP_USERNAME",
            body.smtp_username.trim(),
        )
        .map_err(ApiError::internal)?;
        set_env_key(&state.paths.account_env, "SMTP_FROM", &smtp_from)
            .map_err(ApiError::internal)?;
        set_env_key(&state.paths.account_env, "SMTP_TLS", &smtp_tls).map_err(ApiError::internal)?;
        if body.clear_smtp_password {
            remove_env_key(&state.paths.account_env, "SMTP_PASSWORD")
                .map_err(ApiError::internal)?;
        } else if !body.smtp_password.is_empty() {
            set_env_key(
                &state.paths.account_env,
                "SMTP_PASSWORD",
                &body.smtp_password,
            )
            .map_err(ApiError::internal)?;
        }
    }

    let github_id = body.github_client_id.trim().to_string();
    let env = read_env(&state.paths.account_env);
    let current_secret = env.get("GITHUB_CLIENT_SECRET").cloned().unwrap_or_default();
    let github_secret = if body.clear_github_secret {
        String::new()
    } else if !body.github_client_secret.trim().is_empty() {
        body.github_client_secret.trim().to_string()
    } else {
        current_secret
    };
    let github_callback = body.github_callback.trim().to_string();
    if github_id.is_empty() || github_secret.is_empty() {
        for key in ["GITHUB_CLIENT_ID", "GITHUB_CLIENT_SECRET"] {
            let _ = remove_env_key(&state.paths.account_env, key);
        }
    } else {
        let callback = if github_callback.is_empty() {
            format!(
                "{}api/auth/github/callback",
                mailer::account_base_url(&state.paths)
            )
        } else {
            github_callback
        };
        if !valid_callback(&callback) {
            return Err(ApiError::bad("GitHub 回调地址无效"));
        }
        set_env_key(&state.paths.account_env, "GITHUB_CLIENT_ID", &github_id)
            .map_err(ApiError::internal)?;
        set_env_key(
            &state.paths.account_env,
            "GITHUB_CLIENT_SECRET",
            &github_secret,
        )
        .map_err(ApiError::internal)?;
        set_env_key(&state.paths.account_env, "GITHUB_CALLBACK_URL", &callback)
            .map_err(ApiError::internal)?;
    }

    let qq_app_id = body.qq_app_id.trim().to_string();
    let current_qq_key = env.get("QQ_APP_KEY").cloned().unwrap_or_default();
    let qq_app_key = if body.clear_qq_key {
        String::new()
    } else if !body.qq_app_key.trim().is_empty() {
        body.qq_app_key.trim().to_string()
    } else {
        current_qq_key
    };
    let qq_callback = body.qq_callback.trim().to_string();
    if qq_app_id.is_empty() || qq_app_key.is_empty() {
        for key in ["QQ_APP_ID", "QQ_APP_KEY", "QQ_CALLBACK_URL"] {
            let _ = remove_env_key(&state.paths.account_env, key);
        }
    } else {
        let callback = if qq_callback.is_empty() {
            format!(
                "{}api/auth/qq/callback",
                mailer::account_base_url(&state.paths)
            )
        } else {
            qq_callback
        };
        if !valid_callback(&callback) {
            return Err(ApiError::bad("QQ 回调地址无效"));
        }
        set_env_key(&state.paths.account_env, "QQ_APP_ID", &qq_app_id)
            .map_err(ApiError::internal)?;
        set_env_key(&state.paths.account_env, "QQ_APP_KEY", &qq_app_key)
            .map_err(ApiError::internal)?;
        set_env_key(&state.paths.account_env, "QQ_CALLBACK_URL", &callback)
            .map_err(ApiError::internal)?;
    }

    if let Some(days) = body.session_ttl_days {
        if !(1..=365).contains(&days) {
            return Err(ApiError::bad("网页会话时长需要 1-365 天"));
        }
        set_env_key(
            &state.paths.account_env,
            "WEB_SESSION_TTL_SECS",
            &(days * 24 * 60 * 60).to_string(),
        )
        .map_err(ApiError::internal)?;
    }

    let _ = crate::accounts::audit(
        &state.db,
        Some(&actor.user_id),
        "auth.settings",
        Some("account_env"),
        None,
        Some(&format!(
            "smtp={} github={} qq={} sessionDays={}",
            !smtp_host.is_empty(),
            !github_id.is_empty() && !github_secret.is_empty(),
            !qq_app_id.is_empty() && !qq_app_key.is_empty(),
            body.session_ttl_days.map(|value| value.to_string()).unwrap_or_else(|| "unchanged".to_string())
        )),
    )
    .await;

    Ok(Json(json!({
        "ok": true,
        "message": "认证配置已保存，立即生效，无需重启",
        "state": state_value(&state),
    })))
}

async fn test_email(State(state): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    let value = json_body(body).await?;
    let body: TestEmailBody = serde_json::from_value(value)
        .map_err(|error| ApiError::bad(format!("请求格式错误：{error}")))?;
    let to = body.to.trim();
    if !to.contains('@') {
        return Err(ApiError::bad("请输入有效的收件邮箱"));
    }
    if !mailer::configured(&state.paths) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SMTP 尚未配置",
        ));
    }
    mailer::send_test(&state.paths, to)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, error.to_string()))?;
    Ok(Json(json!({"ok": true, "message": "测试邮件已发送"})))
}

async fn test_github(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let settings = oauth::GithubSettings::load(&state.paths)
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "GitHub OAuth 尚未配置"))?;
    let url = settings
        .authorize_url("sukisu-config-test")
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "message": "GitHub OAuth 参数有效",
        "callback": settings.callback,
        "authorizeUrl": url,
    })))
}
