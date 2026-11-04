// Helper APIs are kept for the standalone service and future protocol extensions.
#![allow(dead_code)]

mod account_api;
mod accounts;
mod admin_users;
mod api;
mod auth;
mod auth_settings_api;
mod authors;
mod catalog;
mod comments;
mod config;
mod db;
mod follows;
mod images;
mod jobs;
mod mailer;
mod mirrors;
mod models;
mod network_api;
mod oauth;
mod package;
mod qq;
mod resumable;
mod sources;
mod sources_api;
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
    resumable::cleanup_stale(&state.paths);

    if let Err(error) = accounts::ensure_admin_seed(&state.db, &state.paths).await {
        tracing::warn!("admin seed warning: {error:#}");
    }
    if let Err(error) = accounts::ensure_super_admin(&state.db, &state.paths).await {
        tracing::warn!("super admin seed warning: {error:#}");
    }
    if let Err(error) = authors::import_legacy_authors(&state.db, &state.paths).await {
        tracing::warn!("author import warning: {error:#}");
    }
    if let Err(error) = authors::revoke_inactive_authors(&state.db, &state.paths).await {
        tracing::warn!("author auto-revoke warning: {error:#}");
    }
    let flush_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            flush_state.flush_downloads().await;
        }
    });
    let mirror_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            mirror_state.mirrors.reload().await;
        }
    });
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) =
                catalog::purge_expired_withdrawn(&cleanup_state.store, &cleanup_state.paths).await
            {
                tracing::warn!("withdrawn cleanup warning: {error:#}");
            }
            cleanup_state.refresh_withdrawn_assets();
            resumable::cleanup_stale(&cleanup_state.paths);
            cleanup_state.flush_downloads().await;
            if let Err(error) = catalog::refresh_catalog_stats(&cleanup_state.paths) {
                tracing::warn!("catalog stats refresh warning: {error:#}");
            }
            if let Err(error) = comments::purge_expired(&cleanup_state.db).await {
                tracing::warn!("comment cleanup warning: {error:#}");
            }
            if let Err(error) =
                authors::revoke_inactive_authors(&cleanup_state.db, &cleanup_state.paths).await
            {
                tracing::warn!("author auto-revoke warning: {error:#}");
            }
            cleanup_state.reload_theme_stats();
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    if let Err(error) = comments::purge_expired(&state.db).await {
        tracing::warn!("comment cleanup warning: {error:#}");
    }
    if let Err(error) = catalog::backfill_meta_assets(&state.store, &state.paths).await {
        tracing::warn!("storage init warning: {error:#}");
    }
    // Rebuild when the catalog is missing or its maintenance flag no longer matches config.env,
    // so a manual config edit or an old binary cannot leave the flag stale.
    let maintenance = config::maintenance_enabled(&state.paths);
    let catalog_stale = catalog::read_catalog(&state.paths)
        .map(|catalog| catalog.maintenance != maintenance)
        .unwrap_or(true);
    if catalog_stale {
        if let Err(error) = catalog::rebuild_catalog(&state.store, &state.paths).await {
            tracing::warn!("catalog init rebuild warning: {error:#}");
        }
    }
    if let Err(error) = catalog::refresh_catalog_stats(&state.paths) {
        tracing::warn!("catalog stats init warning: {error:#}");
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
