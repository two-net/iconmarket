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
use crate::{ai, line, notifications, persistence, scheduler, shutdown, webhook};

/// Runs the steps after config loading and validation, which `main` has already done.
///
/// A failure before rich-menu provisioning stops startup.
/// Returns once the server has shut down.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let db = persistence::connect(&config.database)
        .await
        .context("cannot connect to the database")?;
    info!("connected to the database");
    persistence::check_encoding(&db).await?;

    let migration = persistence::migrate(&db)
        .await
        .context("auto-migration failed")?;
    for object in &migration.added {
        info!(object = %object, "created in the database schema");
    }
    for column in &migration.relaxed {
        warn!(
            column = %column,
            "column kept for a removed model property, now nullable"
        );
    }
    info!(
        snapshot = %migration.snapshot,
        added = migration.added.len(),
        relaxed = migration.relaxed.len(),
        "database schema is up to date"
    );

    let seeded = persistence::seed(&db, &config.catalog, &config.admin)
        .await
        .context("seeding failed")?;
    info!(
        categories = seeded.categories,
        admins_granted = seeded.granted,
        admins_revoked = seeded.revoked,
        "seed data applied"
    );

    // Turns off, with a warning, the AI features the model can't serve (FR-AIP-04). The
    // dispatcher (#14) takes it over.
    let _ai = ai::Gateway::new(&config.ai, config.app.time_zone, db.clone());

    let line = line::Client::new(&config.line)?;

    // A rich-menu failure is logged and startup continues
    if let Err(e) = notifications::provision_rich_menus(&line).await {
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
    let app = http::router(AppState {
        db: db.clone(),
        webhook: webhook::Verifier::new(&config.line.channel_secret),
    });
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
