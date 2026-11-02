use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

fn systemctl_disabled() -> bool {
    std::env::var("THEME_DISABLE_SYSTEMCTL")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn detached(unit: &str, delay_seconds: u32, command: &str) {
    if !cfg!(target_os = "linux") || systemctl_disabled() {
        tracing::info!("skipping {unit} restart (non-linux or disabled)");
        return;
    }
    let unit_name = format!("{unit}-{}", crate::util::random_hex(3));
    let result = std::process::Command::new("systemd-run")
        .args([
            "--on-active",
            &delay_seconds.to_string(),
            "--collect",
            "--unit",
            &unit_name,
            "sh",
            "-c",
            command,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(error) = result {
        tracing::warn!("failed to schedule {unit} restart: {error}");
    }
}

pub fn schedule_admin_restart() {
    detached(
        "sukisu-admin-restart",
        2,
        "systemctl restart sukisu-theme-admin",
    );
}

pub fn schedule_caddy_restart() {
    detached("sukisu-caddy-restart", 1, "systemctl restart caddy");
}

pub async fn service_state(unit: &str) -> String {
    if !cfg!(target_os = "linux") || systemctl_disabled() {
        return "unknown".to_string();
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Command::new("systemctl").args(["is-active", unit]).output(),
    )
    .await;
    match output {
        Ok(Ok(output)) => {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if value.is_empty() {
                "unknown".to_string()
            } else {
                value
            }
        }
        _ => "unknown".to_string(),
    }
}

pub async fn reload_caddy() -> Result<()> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        Command::new("systemctl")
            .args(["reload", "caddy"])
            .output(),
    )
    .await
    .context("systemctl reload timed out")?
    .context("failed to run systemctl")?;
    if !output.status.success() {
        bail!(
            "systemctl reload failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub async fn hash_password(password: &str) -> Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Command::new("caddy")
            .args(["hash-password", "--plaintext", password])
            .output(),
    )
    .await
    .context("caddy hash-password timed out")?
    .context("failed to run caddy hash-password")?;
    if !output.status.success() {
        bail!(
            "caddy hash-password failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hash.is_empty() {
        bail!("caddy hash-password returned no output");
    }
    Ok(hash)
}
