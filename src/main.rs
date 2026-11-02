mod api;
mod catalog;
mod config;
mod images;
mod jobs;
mod models;
mod package;
mod storage;
mod system;
mod uploads;
mod util;

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
        .unwrap_or_else(|_| PathBuf::from("/etc/sukisu-theme-admin/storage.env"));
    let settings = config::read_storage_config(&storage_env);
    let state = api::AppState::new(settings)?;

    if let Err(error) = catalog::backfill_meta_assets(&state.store, &state.paths).await {
        tracing::warn!("storage init warning: {error:#}");
    }

    let app = api::router(state);
    let port = std::env::var("ADMIN_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8090);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("sukisu-theme-admin listening on 127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}
