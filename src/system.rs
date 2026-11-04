use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use tokio::process::Command;

fn service_cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn systemctl_disabled() -> bool {
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

pub fn schedule_network_apply(
    script: &str,
    new_port: u16,
    old_port: u16,
    backup_dir: &str,
    health_host: &str,
) -> anyhow::Result<()> {
    if cfg!(not(target_os = "linux")) || systemctl_disabled() {
        return Ok(());
    }
    let unit_name = format!("sukisu-network-{}", crate::util::random_hex(3));
    let status = std::process::Command::new("systemd-run")
        .args([
            "--on-active",
            "2",
            "--collect",
            "--unit",
            &unit_name,
            script,
            &new_port.to_string(),
            &old_port.to_string(),
            backup_dir,
            health_host,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to schedule network apply")?;
    if !status.success() {
        bail!("systemd-run failed to schedule network apply");
    }
    Ok(())
}

pub fn schedule_admin_restart() {
    detached(
        "sukisu-admin-restart",
        2,
        "systemctl restart sukisu-theme-web",
    );
}

pub async fn service_state(unit: &str) -> String {
    if !cfg!(target_os = "linux") || systemctl_disabled() {
        return "unknown".to_string();
    }
    if let Ok(cache) = service_cache().lock() {
        if let Some((at, value)) = cache.get(unit) {
            if at.elapsed() < Duration::from_secs(5) {
                return value.clone();
            }
        }
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Command::new("systemctl").args(["is-active", unit]).output(),
    )
    .await;
    let value = match output {
        Ok(Ok(output)) => {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if value.is_empty() {
                "unknown".to_string()
            } else {
                value
            }
        }
        _ => "unknown".to_string(),
    };
    if let Ok(mut cache) = service_cache().lock() {
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(unit.to_string(), (Instant::now(), value.clone()));
    }
    value
}
