use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{read_env, Paths};
use crate::db::Database;
use crate::util::now_iso;

#[derive(Clone, Debug)]
pub struct QqSettings {
    pub app_id: String,
    pub app_key: String,
    pub callback: String,
}

impl QqSettings {
    pub fn load(paths: &Paths) -> Option<Self> {
        let env = read_env(&paths.account_env);
        let app_id = env
            .get("QQ_APP_ID")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let app_key = env
            .get("QQ_APP_KEY")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let callback = env
            .get("QQ_CALLBACK_URL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "{}api/auth/qq/callback",
                    crate::mailer::account_base_url(paths)
                )
            });
        Some(Self {
            app_id,
            app_key,
            callback,
        })
    }

    pub fn authorize_url(&self, state: &str) -> Result<String> {
        let mut url = reqwest::Url::parse("https://graph.qq.com/oauth2.0/authorize")
            .context("QQ OAuth URL 无效")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.app_id)
            .append_pair("redirect_uri", &self.callback)
            .append_pair("state", state)
            .append_pair("scope", "get_user_info");
        Ok(url.into())
    }
}

#[derive(Clone, Debug)]
pub struct QqProfile {
    pub openid: String,
    /// QQ does not expose an email address to third-party apps.
    pub nickname: String,
    pub avatar: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<Value>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct OpenIdResponse {
    openid: Option<String>,
    error: Option<Value>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserInfoResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    nickname: String,
    #[serde(rename = "figureurl_qq_2", default)]
    avatar: Option<String>,
}

pub async fn exchange(http: &Client, settings: &QqSettings, code: &str) -> Result<QqProfile> {
    let token_response = http
        .get("https://graph.qq.com/oauth2.0/token")
        .query(&[
            ("grant_type", "authorization_code"),
            ("client_id", settings.app_id.as_str()),
            ("client_secret", settings.app_key.as_str()),
            ("code", code),
            ("redirect_uri", settings.callback.as_str()),
            ("fmt", "json"),
        ])
        .header("User-Agent", "sukisu-theme-web")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("请求 QQ token 失败")?
        .error_for_status()
        .context("QQ token 接口返回错误")?
        .json::<TokenResponse>()
        .await
        .context("解析 QQ token 响应失败")?;
    let token = token_response
        .access_token
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "QQ 授权失败: {}",
                token_response
                    .error_description
                    .or_else(|| token_response.error.map(|value| value.to_string()))
                    .unwrap_or_else(|| "missing access token".to_string())
            )
        })?;

    let openid_response = http
        .get("https://graph.qq.com/oauth2.0/me")
        .query(&[("access_token", token.as_str()), ("fmt", "json")])
        .header("User-Agent", "sukisu-theme-web")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("读取 QQ openid 失败")?
        .error_for_status()
        .context("QQ openid 接口返回错误")?
        .json::<OpenIdResponse>()
        .await
        .context("解析 QQ openid 响应失败")?;
    let openid = openid_response
        .openid
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "QQ openid 获取失败: {}",
                openid_response
                    .error_description
                    .or_else(|| openid_response.error.map(|value| value.to_string()))
                    .unwrap_or_else(|| "missing openid".to_string())
            )
        })?;

    let user = http
        .get("https://graph.qq.com/user/get_user_info")
        .query(&[
            ("access_token", token.as_str()),
            ("oauth_consumer_key", settings.app_id.as_str()),
            ("openid", openid.as_str()),
            ("fmt", "json"),
        ])
        .header("User-Agent", "sukisu-theme-web")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("读取 QQ 用户信息失败")?
        .error_for_status()
        .context("QQ 用户信息接口返回错误")?
        .json::<UserInfoResponse>()
        .await
        .context("解析 QQ 用户信息失败")?;
    if user.ret != 0 {
        return Err(anyhow!(
            "QQ 用户信息获取失败: {} ({})",
            user.msg,
            user.ret
        ));
    }
    let nickname = user.nickname.trim().to_string();
    Ok(QqProfile {
        openid,
        nickname: if nickname.is_empty() {
            "QQ 用户".to_string()
        } else {
            nickname
        },
        avatar: user.avatar.filter(|value| !value.trim().is_empty()),
    })
}

pub async fn identity_user_id(db: &Database, openid: &str) -> Result<Option<String>> {
    let openid = openid.to_string();
    db.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT user_id FROM user_identities
                 WHERE provider = 'qq' AND provider_user_id = ?1",
                [openid],
                |row| row.get(0),
            )
            .optional()?)
    })
    .await
}

pub async fn link_identity(db: &Database, user_id: &str, profile: &QqProfile) -> Result<()> {
    let user_id = user_id.to_string();
    let openid = profile.openid.clone();
    let nickname = profile.nickname.clone();
    let now = now_iso();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO user_identities(user_id, provider, provider_user_id, provider_login, email, created_at)
             VALUES (?1, 'qq', ?2, ?3, NULL, ?4)
             ON CONFLICT(provider, provider_user_id) DO UPDATE SET
                 provider_login = excluded.provider_login",
            params![user_id, openid, nickname, now],
        )?;
        Ok(())
    })
    .await
}
