//! The startup sequence (NFR-MNT-06), serving, and graceful shutdown.

use std::future::IntoFuture;
use std::net::Ipv4Addr;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

use crate::config::Config;
use crate::http::{self, AppState};
use crate::notifications::templates::Templates;
use crate::{ai, dispatcher, line, notifications, persistence, scheduler, shutdown, webhook};

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

    // Turns off, with a warning, the AI features the model can't serve (FR-AIP-04)
    let ai = ai::Gateway::new(&config.ai, config.app.time_zone, db.clone());

    let line = line::Client::new(&config.line)?;

    // A rich-menu failure is logged and startup continues
    if let Err(e) = notifications::provision_rich_menus(&line).await {
        warn!(
            error = format!("{e:#}"),
            "rich-menu provisioning failed, continuing"
        );
    }
    // Users whose roles changed, by the admin sync or while the app was down, get their menu
    let orm = persistence::orm(db.clone());
    if let Err(e) = notifications::relink_rich_menus(&orm, &line, None).await {
        warn!(
            error = format!("{e:#}"),
            "rich-menu relinking failed, continuing"
        );
    }

    let stop = CancellationToken::new();
    let jobs = TaskTracker::new();
    scheduler::start(
        &jobs,
        stop.clone(),
        db.clone(),
        line.clone(),
        config.app.time_zone,
    )
    .await;
    // The dispatcher keeps what it needs of the config, the rest of which is read by now
    let port = config.app.listen_port;
    let webhook = webhook::Verifier::new(&config.line.channel_secret);
    let wakeup = dispatcher::Wakeup::default();
    let context = dispatcher::Context {
        db: orm,
        line,
        ai,
        templates: Templates::new(&config.app),
        config: Arc::new(config),
    };
    dispatcher::start(&jobs, stop.clone(), context, wakeup.clone());

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
        .await
        .with_context(|| format!("cannot listen on port {port}"))?;
    let app = http::router(AppState {
        db: db.clone(),
        webhook,
        wakeup,
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
