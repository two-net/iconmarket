//! Persistence (SRS §2.1): the PostgreSQL connection pool, the code-first entity models
//! (§5.1), auto-migration and seeding (§5.3).
//!
//! The ORM is SeaORM. The models in [`entity`] are the single source of truth for the schema
//! (CON-04, MIG-01): [`schema`] generates every table, key, index and constraint from them,
//! and [`migrate`] applies it at every startup (MIG-02).
//!
//! Auto-migration only adds (MIG-03), so a model change must be additive too. A column added
//! to an existing table must be nullable or have a `default_value`, or migrating a table that
//! has rows fails. A removed property leaves its column in place, made nullable. A model change
//! that would drop or rename anything else stops startup instead; such a change needs a
//! deliberately reviewed clean-up release.

pub mod entity;
mod migration;
mod money;
mod seed;
#[cfg(test)]
mod tests;

use std::time::Duration;

use anyhow::{Context, ensure};
use sea_orm::prelude::Uuid;
use sea_orm::schema::SchemaBuilder;
use sea_orm::{DatabaseConnection, DbBackend, EntityTrait, Schema, SqlxPostgresConnector};
use sqlx::ConnectOptions;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::config::DatabaseConfig;

pub use migration::migrate;
pub use seed::seed;

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

/// Every table of §5.1 and the schema versions (MIG-09), with their keys, indexes and
/// constraints, as the entity models define them.
pub fn schema() -> SchemaBuilder {
    add_entities(Schema::new(DbBackend::Postgres).builder())
}

/// Something built from every entity model by [`add_entities`].
trait FromEntities {
    fn add<E: EntityTrait>(self, entity: E) -> Self;
}

impl FromEntities for SchemaBuilder {
    fn add<E: EntityTrait>(self, entity: E) -> Self {
        self.register(entity)
    }
}

/// Adds every entity model to `models`: the one list of them.
fn add_entities<T: FromEntities>(models: T) -> T {
    use entity::*;

    // Order doesn't matter: tables are created after those they reference
    models
        .add(user::Entity)
        .add(shop::Entity)
        .add(category::Entity)
        .add(product::Entity)
        .add(image::Entity)
        .add(cart_item::Entity)
        .add(order::Entity)
        .add(order_item::Entity)
        .add(order_event::Entity)
        .add(payment_slip::Entity)
        .add(rating::Entity)
        .add(report::Entity)
        .add(session::Entity)
        .add(pending_action::Entity)
        .add(ai_message::Entity)
        .add(ai_call_log::Entity)
        .add(webhook_event::Entity)
        .add(push_log::Entity)
        .add(audit_log::Entity)
        .add(counter::Entity)
        .add(schema_version::Entity)
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
