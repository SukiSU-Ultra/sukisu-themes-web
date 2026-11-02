mod account_api;
mod accounts;
mod admin_users;
mod api;
mod auth;
mod auth_settings_api;
mod authors;
mod catalog;
mod config;
mod db;
mod images;
mod jobs;
mod mailer;
mod models;
mod network_api;
mod oauth;
mod package;
mod storage;
mod system;
mod uploads;
mod util;

use std::net::IpAddr;
use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,sukisu_theme_admin=debug")),
        )
        .with_target(false)
        .init();

    let storage_env = std::env::var("THEME_STORAGE_ENV")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/sukisu-theme-web/storage.env"));
    let settings = config::read_storage_config(&storage_env);
    let state = api::AppState::new(settings).await?;

    if let Err(error) = accounts::ensure_admin_seed(&state.db, &state.paths).await {
        tracing::warn!("admin seed warning: {error:#}");
    }
    if let Err(error) = authors::import_legacy_authors(&state.db, &state.paths).await {
        tracing::warn!("author import warning: {error:#}");
    }
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) =
                catalog::purge_expired_withdrawn(&cleanup_state.store, &cleanup_state.paths).await
            {
                tracing::warn!("withdrawn cleanup warning: {error:#}");
            }
            cleanup_state.refresh_withdrawn_assets();
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    if let Err(error) = catalog::backfill_meta_assets(&state.store, &state.paths).await {
        tracing::warn!("storage init warning: {error:#}");
    }
    state.refresh_withdrawn_assets();

    let network = config::read_network_settings(&state.paths);
    let bind: IpAddr = network
        .bind
        .parse()
        .map_err(|_| anyhow::anyhow!("无效的 ADMIN_BIND: {}", network.bind))?;
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind((bind, network.port)).await?;
    tracing::info!("sukisu-theme-web listening on {bind}:{}", network.port);
    axum::serve(listener, app).await?;
    Ok(())
}
