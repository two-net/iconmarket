//! Persistence (SRS §2.1): the PostgreSQL connection pool, the code-first entity models
//! (§5.1), auto-migration and seeding. Migration and seeding arrive with #7.
//!
//! The ORM is SeaORM. The models in [`entity`] are the single source of truth for the schema
//! (CON-04, MIG-01): [`schema`] generates every table, key, index and constraint from them.

pub mod entity;
mod money;
#[cfg(test)]
mod tests;

use std::time::Duration;

use anyhow::{Context, ensure};
use sea_orm::prelude::Uuid;
use sea_orm::schema::SchemaBuilder;
use sea_orm::{DatabaseConnection, DbBackend, Schema, SqlxPostgresConnector};
use sqlx::ConnectOptions;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing::debug;

use crate::config::DatabaseConfig;

/// How long `/health` waits for the database before reporting it down.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Removes libpq's `PG*` environment variables.
///
/// sqlx fills connection options from them, and some (like `PGOPTIONS`) can't be unset
/// afterwards. Configuration comes only from the config file (CFG-02, CON-09).
///
/// Call it first thing in `main`, before any other thread exists.
pub fn clear_libpq_env() {
    let names: Vec<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.as_encoded_bytes().starts_with(b"PG"))
        .collect();
    for name in names {
        // SAFETY: runs before any other thread exists, so nothing reads the environment concurrently
        unsafe { std::env::remove_var(name) };
    }
}

pub async fn connect(config: &DatabaseConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(connect_options(config))
        .await
}

fn connect_options(config: &DatabaseConfig) -> PgConnectOptions {
    // Every option comes from the config file; `new_without_pgpass` skips ~/.pgpass
    PgConnectOptions::new_without_pgpass()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .password(config.password.expose())
        .database(&config.name)
        .ssl_mode(config.ssl_mode)
        .application_name("icon-market")
        .disable_statement_logging()
}

/// The ORM over `pool`, so every query is parameterized (NFR-SEC-07).
#[allow(
    dead_code,
    reason = "the flows that query through the ORM are built by later issues"
)]
pub fn orm(pool: PgPool) -> DatabaseConnection {
    SqlxPostgresConnector::from_sqlx_postgres_pool(pool)
}

/// Fails unless the database stores text as UTF-8, so Thai is kept and compared without loss
/// (DR-09).
pub async fn check_encoding(db: &PgPool) -> anyhow::Result<()> {
    let encoding: String = sqlx::query_scalar("SHOW server_encoding")
        .fetch_one(db)
        .await
        .context("cannot read the database encoding")?;
    ensure!(
        encoding == "UTF8",
        "the database encoding is {encoding}, but Thai text needs UTF8 (DR-09)"
    );
    Ok(())
}

/// Whether the database answers within [`PING_TIMEOUT`].
pub async fn ping(db: &PgPool) -> bool {
    let query = sqlx::query("SELECT 1").execute(db);
    matches!(tokio::time::timeout(PING_TIMEOUT, query).await, Ok(Ok(_)))
}

/// Every table of §5.1, with its keys, indexes and constraints, as the entity models define it.
#[allow(
    dead_code,
    reason = "auto-migration applies it from #7; only tests do so far"
)]
pub fn schema() -> SchemaBuilder {
    use entity::*;

    // Registration order doesn't matter: tables are created after those they reference
    Schema::new(DbBackend::Postgres)
        .builder()
        .register(user::Entity)
        .register(shop::Entity)
        .register(category::Entity)
        .register(product::Entity)
        .register(image::Entity)
        .register(cart_item::Entity)
        .register(order::Entity)
        .register(order_item::Entity)
        .register(order_event::Entity)
        .register(payment_slip::Entity)
        .register(rating::Entity)
        .register(report::Entity)
        .register(session::Entity)
        .register(pending_action::Entity)
        .register(ai_message::Entity)
        .register(ai_call_log::Entity)
        .register(webhook_event::Entity)
        .register(push_log::Entity)
        .register(audit_log::Entity)
        .register(counter::Entity)
}

/// A new ID for anything reachable by a public link or carried in postback data: 128 bits
/// from the OS random source (NFR-SEC-04). A v4 UUID would have only 122 random bits.
#[allow(
    dead_code,
    reason = "images and pending actions are created by later issues"
)]
pub fn random_id() -> Uuid {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).expect("the OS random source is available");
    Uuid::from_bytes(bytes)
}

/// Brings the schema up to date with the entity models (MIG-02).
pub async fn migrate(_db: &PgPool) -> anyhow::Result<()> {
    debug!("auto-migration not implemented yet (#7)");
    Ok(())
}

/// Seeds the default categories and syncs admin roles from the config (MIG-05, FR-ADM-01).
pub async fn seed(_db: &PgPool) -> anyhow::Result<()> {
    debug!("seeding and admin sync not implemented yet (#7)");
    Ok(())
}
