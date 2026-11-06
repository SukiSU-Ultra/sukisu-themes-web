// Helper APIs are kept for the standalone service and future protocol extensions.
#![allow(dead_code)]

mod account_api;
mod accounts;
mod admin_users;
mod api;
mod api_monitor;
mod auth;
mod auth_settings_api;
mod authors;
mod catalog;
mod client_access;
mod comments;
mod community;
mod config;
mod db;
mod distribution;
mod edge;
mod follows;
mod images;
mod jobs;
mod mailer;
mod manager_version;
mod mirrors;
mod models;
mod net;
mod network_api;
mod oauth;
mod package;
mod performance;
mod performance_api;
mod resumable;
mod site;
mod sources;
mod sources_api;
mod storage;
mod system;
mod theme_tokens;
mod upload_ingress;
mod uploads;
mod util;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,sukisu_theme_admin=debug")),
        )
        .with_target(false)
        .init();

    if std::env::args().nth(1).as_deref() == Some("edge") {
        let file = std::env::args_os()
            .nth(2)
            .ok_or_else(|| anyhow::anyhow!("用法: sukisu-theme-web edge <config.json>"))?;
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(edge::run(std::path::Path::new(&file)));
    }

    if std::env::args().nth(1).as_deref() == Some("site") {
        let file = std::env::args_os()
            .nth(2)
            .ok_or_else(|| anyhow::anyhow!("用法: sukisu-theme-web site <config.json>"))?;
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(site::run(std::path::Path::new(&file)));
    }

    // Keep the lock until the runtime (including background DB tasks) is dropped.
    let _writer_lock = site::lock_child()?;
    let storage_env = std::env::var("THEME_STORAGE_ENV")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/sukisu-theme-web/storage.env"));
    let settings = config::read_storage_config(&storage_env);
    let paths = config::load_paths(&settings);
    if std::env::args().nth(1).as_deref() == Some("migrate-db") {
        let report = db::migrate_split(&paths)?;
        println!("数据库已拆分到 {}", report.target);
        for (table, rows) in report.tables {
            println!("  {table}: {rows}");
        }
        return Ok(());
    }
    let performance = config::read_performance_settings(&paths);

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().max_blocking_threads(performance.blocking_threads);
    if performance.worker_threads > 0 {
        builder.worker_threads(performance.worker_threads);
    }
    let runtime = builder.build()?;
    runtime.block_on(run(settings))
}

async fn run(settings: config::StorageSettings) -> anyhow::Result<()> {
    let state = api::AppState::new(settings).await?;
    site::watch_child(state.clone());
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
    match authors::reconcile_stale_author_requests(&state.db).await {
        Ok(updated) if !updated.is_empty() => {
            tracing::info!(count = updated.len(), "reconciled stale author requests");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!("author request reconcile warning: {error:#}"),
    }
    if let Err(error) = authors::revoke_inactive_authors(&state.db, &state.paths).await {
        tracing::warn!("author auto-revoke warning: {error:#}");
    }
    if let Err(error) = state.db.repair_orphans().await {
        tracing::warn!("database orphan repair warning: {error:#}");
    }
    let flush_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_flush = std::time::Instant::now();
        loop {
            tick.tick().await;
            let Some(_background) = site::background().await else {
                break;
            };
            if last_flush.elapsed() >= flush_state.flush_interval() {
                flush_state.flush_downloads().await;
                last_flush = std::time::Instant::now();
            }
        }
    });
    let mirror_state = state.clone();
    let distribution_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(_background) = site::background().await else {
                break;
            };
            distribution_state.distribution.probe().await;
        }
    });
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(_background) = site::background().await else {
                break;
            };
            mirror_state.mirrors.reload().await;
        }
    });
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        loop {
            let Some(_background) = site::background().await else {
                break;
            };
            match catalog::purge_expired_withdrawn(&cleanup_state.store, &cleanup_state.paths).await
            {
                Ok(ids) => {
                    for id in ids {
                        if let Err(error) = community::remove_theme_activity(&cleanup_state.db, &id).await {
                            tracing::warn!(theme_id = %id, "withdrawn community cleanup warning: {error:#}");
                        }
                        if let Err(error) = comments::clear_theme(&cleanup_state.db, &id).await {
                            tracing::warn!(theme_id = %id, "withdrawn comment cleanup warning: {error:?}");
                        }
                    }
                }
                Err(error) => tracing::warn!("withdrawn cleanup warning: {error:#}"),
            }
            cleanup_state.refresh_withdrawn_assets();
            resumable::cleanup_stale(&cleanup_state.paths);
            cleanup_state.flush_downloads().await;
            if let Err(error) = cleanup_state.db.reclaim_space().await {
                tracing::warn!("database reclaim warning: {error:#}");
            }
            if let Err(error) = cleanup_state.db.repair_orphans().await {
                tracing::warn!("database orphan repair warning: {error:#}");
            }
            if let Err(error) = catalog::refresh_catalog_stats(&cleanup_state.paths) {
                tracing::warn!("catalog stats refresh warning: {error:#}");
            }
            if let Err(error) = comments::purge_expired(&cleanup_state.db).await {
                tracing::warn!("comment cleanup warning: {error:#}");
            }
            if let Err(error) = authors::reconcile_stale_author_requests(&cleanup_state.db).await {
                tracing::warn!("author request reconcile warning: {error:#}");
            }
            if let Err(error) =
                authors::revoke_inactive_authors(&cleanup_state.db, &cleanup_state.paths).await
            {
                tracing::warn!("author auto-revoke warning: {error:#}");
            }
            cleanup_state.reload_theme_stats();
            drop(_background);
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
    match network.ip_family {
        net::IpFamily::Ipv4 if !bind.is_ipv4() => {
            anyhow::bail!("ADMIN_IP_FAMILY=ipv4，但 ADMIN_BIND={} 不是 IPv4", network.bind)
        }
        net::IpFamily::Ipv6 if !bind.is_ipv6() => {
            anyhow::bail!("ADMIN_IP_FAMILY=ipv6，但 ADMIN_BIND={} 不是 IPv6", network.bind)
        }
        _ => {}
    }
    let app = api::router(state.clone());
    let listener = tokio::net::TcpListener::bind((bind, network.port)).await?;
    tracing::info!("sukisu-theme-web listening on {bind}:{}", network.port);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(site::shutdown_child())
    .await?;
    state.flush_downloads().await;
    Ok(())
}
