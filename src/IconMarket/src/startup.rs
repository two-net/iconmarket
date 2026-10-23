//! The startup sequence (NFR-MNT-06), serving, and graceful shutdown.

use std::future::IntoFuture;
use std::net::Ipv4Addr;

use anyhow::{Context, anyhow};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

use crate::config::Config;
use crate::http::{self, AppState};
use crate::{notifications, persistence, scheduler, shutdown};

/// Runs the steps after config loading and validation, which `main` has already done.
///
/// A failure before rich-menu provisioning stops startup.
/// Returns once the server has shut down.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let db = persistence::connect(&config.database)
        .await
        .context("cannot connect to the database")?;
    info!("connected to the database");

    persistence::migrate(&db)
        .await
        .context("auto-migration failed")?;
    persistence::seed(&db).await.context("seeding failed")?;

    // A rich-menu failure is logged and startup continues
    if let Err(e) = notifications::provision_rich_menus().await {
        warn!(
            error = format!("{e:#}"),
            "rich-menu provisioning failed, continuing"
        );
    }

    let stop = CancellationToken::new();
    let jobs = TaskTracker::new();
    scheduler::start(&jobs, stop.clone());

    let port = config.app.listen_port;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
        .await
        .with_context(|| format!("cannot listen on port {port}"))?;
    let app = http::router(AppState { db: db.clone() });
    let mut server = tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.clone().cancelled_owned())
            .into_future(),
    );
    info!(port, "accepting requests");

    tokio::select! {
        () = shutdown::signal() => info!("shutdown requested"),
        result = &mut server => {
            return match result {
                Ok(Ok(())) => Err(anyhow!("server stopped unexpectedly")),
                Ok(Err(e)) => Err(e).context("server failed"),
                Err(e) => Err(e).context("server task failed"),
            };
        }
    }

    // Stop accepting connections and tell jobs to stop picking up events
    stop.cancel();
    if shutdown::drain(server, &jobs, shutdown::DRAIN_LIMIT).await {
        db.close().await;
        info!("shutdown complete");
    } else {
        warn!(
            limit_secs = shutdown::DRAIN_LIMIT.as_secs(),
            "in-flight work did not finish in time; unprocessed events stay in the inbox"
        );
    }
    Ok(())
}
