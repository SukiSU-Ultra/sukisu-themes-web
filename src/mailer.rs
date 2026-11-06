use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::{read_env, Paths};

tokio::task_local! {
    static REQUEST_ACCOUNT_BASE: String;
}

/// Accepts the public root URL forwarded by a site peer and turns it into the
/// account base used in verification/reset links. Only http(s) roots are valid.
pub(crate) fn normalize_account_base(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return None;
    }
    let base = trimmed.trim_end_matches('/');
    if base.len() < 8 || base.chars().any(char::is_whitespace) {
        return None;
    }
    Some(format!("{base}/account/"))
}

pub(crate) async fn scope_account_base<F>(base: Option<String>, future: F) -> F::Output
where
    F: std::future::Future,
{
    match base.filter(|value| !value.is_empty()) {
        Some(base) => REQUEST_ACCOUNT_BASE.scope(base, future).await,
        None => future.await,
    }
}

#[derive(Clone, Debug)]
pub struct MailSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    pub tls: String,
}

impl MailSettings {
    pub fn load(paths: &Paths) -> Option<Self> {
        let env = read_env(&paths.account_env);
        let host = env.get("SMTP_HOST").map(|value| value.trim()).unwrap_or("");
        if host.is_empty() {
            return None;
        }
        let from = env
            .get("SMTP_FROM")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| env.get("SMTP_USERNAME").cloned().unwrap_or_default());
        if from.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_string(),
            port: env
                .get("SMTP_PORT")
                .and_then(|value| value.trim().parse::<u16>().ok())
                .unwrap_or(587),
            username: env.get("SMTP_USERNAME").cloned().unwrap_or_default(),
            password: env.get("SMTP_PASSWORD").cloned().unwrap_or_default(),
            from,
            tls: env
                .get("SMTP_TLS")
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "starttls".to_string()),
        })
    }

    fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
        let builder = match self.tls.as_str() {
            "none" | "plain" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host),
            "tls" | "ssl" | "wrapper" => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
                .map_err(|error| anyhow!("SMTP 配置失败: {error}"))?,
            _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
                .map_err(|error| anyhow!("SMTP STARTTLS 配置失败: {error}"))?,
        };
        let builder = builder.port(self.port);
        let builder = if self.username.is_empty() {
            builder
        } else {
            builder.credentials(Credentials::new(
                self.username.clone(),
                self.password.clone(),
            ))
        };
        Ok(builder.timeout(Some(Duration::from_secs(20))).build())
    }

    async fn send(&self, to: &str, subject: &str, text: String, html: String) -> Result<()> {
        let message = Message::builder()
            .from(
                self.from
                    .parse()
                    .with_context(|| format!("SMTP_FROM 无效: {}", self.from))?,
            )
            .to(to.parse().with_context(|| format!("收件地址无效: {to}"))?)
            .subject(subject)
            .multipart(lettre::message::MultiPart::alternative_plain_html(
                text, html,
            ))
            .context("构造邮件失败")?;
        self.transport()?
            .send(message)
            .await
            .map_err(|error| anyhow!("发送邮件失败: {error}"))?;
        Ok(())
    }
}

pub fn configured(paths: &Paths) -> bool {
    MailSettings::load(paths).is_some() || dev_mode()
}

fn dev_mode() -> bool {
    std::env::var("THEME_DEV_MODE").as_deref() == Ok("1")
}

pub fn account_base_url(paths: &Paths) -> String {
    if let Ok(base) = REQUEST_ACCOUNT_BASE.try_with(|value| value.clone()) {
        if !base.is_empty() {
            return base;
        }
    }
    configured_account_base_url(paths)
}

/// Configured account base, ignoring any request-scoped site override. Used
/// where the URL must match a fixed registration (e.g. GitHub OAuth callback).
pub(crate) fn configured_account_base_url(paths: &Paths) -> String {
    let account_env = read_env(&paths.account_env);
    if let Some(url) = account_env
        .get("ACCOUNT_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return if url.ends_with('/') {
            url
        } else {
            format!("{url}/")
        };
    }
    let config = read_env(&paths.admin_config);
    let public = config
        .get("PUBLIC_URL")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if public.is_empty() {
        "http://127.0.0.1:8080/account/".to_string()
    } else if public.ends_with('/') {
        format!("{public}account/")
    } else {
        format!("{public}/account/")
    }
}

pub async fn send_verification(paths: &Paths, to: &str, token: &str) -> Result<()> {
    let link = format!(
        "{}api/auth/verify-email?token={}",
        account_base_url(paths),
        token
    );
    let html = format!(
        r#"<div style="font-family:system-ui,sans-serif;max-width:560px;margin:0 auto;padding:28px">
<h2 style="margin:0 0 12px">验证你的 SukiSU 主题镜像账号</h2>
<p style="color:#475569;line-height:1.7">点击下面的按钮完成邮箱验证。链接 24 小时内有效。</p>
<p style="margin:24px 0"><a href="{link}" style="display:inline-block;padding:12px 20px;border-radius:12px;background:#2563eb;color:#fff;text-decoration:none;font-weight:600">验证邮箱</a></p>
<p style="color:#64748b;font-size:12px;line-height:1.7">如果按钮无法点击，请复制这个地址：<br><span style="word-break:break-all">{link}</span></p>
<p style="color:#94a3b8;font-size:12px">如果不是你发起的注册，可以忽略这封邮件。</p>
</div>"#
    );
    let text = format!("验证你的 SukiSU 主题镜像账号：{link}\n链接 24 小时内有效。");
    send_or_log(paths, to, "验证你的 SukiSU 主题镜像账号", text, html, &link).await
}

pub async fn send_password_reset(paths: &Paths, to: &str, token: &str) -> Result<()> {
    let link = format!("{}reset?token={}", account_base_url(paths), token);
    let html = format!(
        r#"<div style="font-family:system-ui,sans-serif;max-width:560px;margin:0 auto;padding:28px">
<h2 style="margin:0 0 12px">重置 SukiSU 主题镜像密码</h2>
<p style="color:#475569;line-height:1.7">点击下面的按钮设置新密码。链接 30 分钟内有效。</p>
<p style="margin:24px 0"><a href="{link}" style="display:inline-block;padding:12px 20px;border-radius:12px;background:#2563eb;color:#fff;text-decoration:none;font-weight:600">设置新密码</a></p>
<p style="color:#64748b;font-size:12px;line-height:1.7">如果按钮无法点击，请复制这个地址：<br><span style="word-break:break-all">{link}</span></p>
<p style="color:#94a3b8;font-size:12px">如果不是你发起的请求，可以忽略这封邮件。</p>
</div>"#
    );
    let text = format!("重置 SukiSU 主题镜像密码：{link}\n链接 30 分钟内有效。");
    send_or_log(paths, to, "重置 SukiSU 主题镜像密码", text, html, &link).await
}

pub async fn send_test(paths: &Paths, to: &str) -> Result<()> {
    let html = r#"<div style="font-family:system-ui,sans-serif;max-width:560px;margin:0 auto;padding:28px">
<h2 style="margin:0 0 12px">SMTP 配置测试</h2>
<p style="color:#475569;line-height:1.7">这是一封来自 SukiSU 主题镜像后台的测试邮件。收到说明 SMTP 配置可用。</p>
</div>"#.to_string();
    let text = "SMTP 配置测试：收到说明 SukiSU 主题镜像后台邮件配置可用。".to_string();
    send_or_log(paths, to, "SukiSU 主题镜像 SMTP 测试", text, html, "").await
}

pub async fn send_notice(paths: &Paths, to: &str, subject: &str, message: &str) -> Result<()> {
    let html = format!(
        r#"<div style="font-family:system-ui,sans-serif;max-width:560px;margin:0 auto;padding:28px">
<h2 style="margin:0 0 12px">{subject}</h2>
<p style="color:#475569;line-height:1.7">{message}</p>
<p style="margin-top:28px"><a href="{}" style="color:#2563eb">打开用户中心</a></p>
</div>"#,
        account_base_url(paths)
    );
    let text = format!("{subject}\n\n{message}\n\n{}", account_base_url(paths));
    send_or_log(paths, to, subject, text, html, "").await
}

async fn send_or_log(
    paths: &Paths,
    to: &str,
    subject: &str,
    text: String,
    html: String,
    dev_link: &str,
) -> Result<()> {
    if let Some(settings) = MailSettings::load(paths) {
        settings.send(to, subject, text, html).await
    } else if dev_mode() {
        tracing::info!("mail dev mode: to={to} subject={subject} link={dev_link}");
        Ok(())
    } else {
        Err(anyhow!("邮件服务尚未配置"))
    }
}
