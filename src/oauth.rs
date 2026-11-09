use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;

use crate::config::{read_env, Paths};
use crate::db::Database;
use crate::util::{now_iso, random_hex, unix_now};

const STATE_TTL_SECS: u64 = 10 * 60;

#[derive(Clone, Debug)]
pub struct GithubSettings {
    pub client_id: String,
    pub client_secret: String,
    pub callback: String,
}

impl GithubSettings {
    pub fn load(paths: &Paths) -> Option<Self> {
        let env = read_env(&paths.account_env);
        let client_id = env
            .get("GITHUB_CLIENT_ID")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let client_secret = env
            .get("GITHUB_CLIENT_SECRET")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let callback = env
            .get("GITHUB_CALLBACK_URL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "{}api/auth/github/callback",
                    crate::mailer::configured_account_base_url(paths)
                )
            });
        Some(Self {
            client_id,
            client_secret,
            callback,
        })
    }

    pub fn authorize_url(&self, state: &str) -> Result<String> {
        let mut url = reqwest::Url::parse("https://github.com/login/oauth/authorize")
            .context("GitHub OAuth URL 无效")?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.callback)
            .append_pair("scope", "read:user user:email")
            .append_pair("state", state);
        Ok(url.into())
    }
}

#[derive(Clone, Debug)]
pub struct GithubProfile {
    pub provider_user_id: String,
    pub login: String,
    pub name: String,
    pub email: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct GithubUser {
    id: i64,
    login: String,
    name: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

pub async fn exchange(
    http: &Client,
    settings: &GithubSettings,
    code: &str,
) -> Result<GithubProfile> {
    let token_response = http
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .header("User-Agent", "sukisu-theme-web")
        .form(&[
            ("client_id", settings.client_id.as_str()),
            ("client_secret", settings.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", settings.callback.as_str()),
        ])
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("请求 GitHub token 失败")?
        .error_for_status()
        .context("GitHub token 接口返回错误")?
        .json::<TokenResponse>()
        .await
        .context("解析 GitHub token 响应失败")?;
    let token = token_response
        .access_token
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "GitHub 授权失败: {}",
                token_response
                    .error_description
                    .or(token_response.error)
                    .unwrap_or_else(|| "missing access token".to_string())
            )
        })?;

    let user = http
        .get("https://api.github.com/user")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sukisu-theme-web")
        .bearer_auth(&token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("读取 GitHub 用户失败")?
        .error_for_status()
        .context("GitHub 用户接口返回错误")?
        .json::<GithubUser>()
        .await
        .context("解析 GitHub 用户失败")?;

    let emails = http
        .get("https://api.github.com/user/emails")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sukisu-theme-web")
        .bearer_auth(&token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("读取 GitHub 邮箱失败")?
        .error_for_status()
        .context("GitHub 邮箱接口返回错误")?
        .json::<Vec<GithubEmail>>()
        .await
        .context("解析 GitHub 邮箱失败")?;
    let email = emails
        .iter()
        .find(|item| item.primary && item.verified)
        .or_else(|| emails.iter().find(|item| item.verified))
        .map(|item| item.email.to_ascii_lowercase())
        .or(user.email.map(|value| value.to_ascii_lowercase()));

    Ok(GithubProfile {
        provider_user_id: user.id.to_string(),
        login: user.login,
        name: user.name.unwrap_or_default(),
        email,
    })
}

pub struct OAuthState {
    pub link_user_id: Option<String>,
    pub redirect: Option<String>,
}

pub async fn create_state(
    db: &Database,
    link_user_id: Option<&str>,
    redirect: Option<&str>,
) -> Result<String> {
    let state = random_hex(24);
    let link_user_id = link_user_id.map(str::to_string);
    let redirect = redirect.map(str::to_string);
    let now = now_iso();
    let expires_at = unix_now() as i64 + STATE_TTL_SECS as i64;
    let state_value = state.clone();
    db.call_write(move |conn| {
        conn.execute(
            "DELETE FROM oauth_states WHERE expires_at <= ?1",
            [unix_now() as i64],
        )?;
        conn.execute(
            "INSERT INTO oauth_states(state, link_user_id, redirect, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![state_value, link_user_id, redirect, expires_at, now],
        )?;
        Ok(())
    })
    .await?;
    Ok(state)
}

pub async fn consume_state(db: &Database, state: &str) -> Result<Option<OAuthState>> {
    let state = state.to_string();
    let now = unix_now() as i64;
    db.call_write(move |conn| {
        let transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row = transaction
            .query_row(
                "SELECT link_user_id, redirect FROM oauth_states
                 WHERE state = ?1 AND expires_at > ?2",
                params![state, now],
                |row| {
                    Ok(OAuthState {
                        link_user_id: row.get(0)?,
                        redirect: row.get(1)?,
                    })
                },
            )
            .optional()?;
        transaction.execute("DELETE FROM oauth_states WHERE state = ?1", [state])?;
        transaction.commit()?;
        Ok(row)
    })
    .await
}

pub async fn identity_user_id(db: &Database, provider_user_id: &str) -> Result<Option<String>> {
    let provider_user_id = provider_user_id.to_string();
    db.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT user_id FROM user_identities
                 WHERE provider = 'github' AND provider_user_id = ?1",
                [provider_user_id],
                |row| row.get(0),
            )
            .optional()?)
    })
    .await
}

pub async fn link_identity(db: &Database, user_id: &str, profile: &GithubProfile) -> Result<()> {
    let user_id = user_id.to_string();
    let provider_user_id = profile.provider_user_id.clone();
    let login = profile.login.clone();
    let email = profile.email.clone();
    let now = now_iso();
    db.call_write(move |conn| {
        conn.execute(
            "INSERT INTO user_identities(user_id, provider, provider_user_id, provider_login, email, created_at)
             VALUES (?1, 'github', ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, provider_user_id) DO UPDATE SET
                 provider_login = excluded.provider_login,
                 email = excluded.email",
            params![user_id, provider_user_id, login, email, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn github_identity(db: &Database, user_id: &str) -> Result<Option<serde_json::Value>> {
    let user_id = user_id.to_string();
    db.call(move |conn| {
        let row = conn
            .query_row(
                "SELECT provider_login, email, created_at FROM user_identities
                 WHERE user_id = ?1 AND provider = 'github'",
                [user_id],
                |row| {
                    Ok(serde_json::json!({
                        "provider": "github",
                        "login": row.get::<_, String>(0)?,
                        "email": row.get::<_, Option<String>>(1)?,
                        "createdAt": row.get::<_, String>(2)?,
                    }))
                },
            )
            .optional()?;
        Ok(row)
    })
    .await
}

pub async fn unlink_github(db: &Database, user_id: &str) -> Result<bool> {
    let user_id = user_id.to_string();
    db.call_write(move |conn| {
        let removed = conn.execute(
            "DELETE FROM user_identities WHERE user_id = ?1 AND provider = 'github'",
            [user_id],
        )?;
        Ok(removed > 0)
    })
    .await
}
