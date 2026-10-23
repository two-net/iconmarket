//! Persistence (SRS §2.1): the PostgreSQL connection pool, auto-migration and seeding.
//! Entity models arrive with #5, migration and seeding with #7.

use std::time::Duration;

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

/// Whether the database answers within [`PING_TIMEOUT`].
pub async fn ping(db: &PgPool) -> bool {
    let query = sqlx::query("SELECT 1").execute(db);
    matches!(tokio::time::timeout(PING_TIMEOUT, query).await, Ok(Ok(_)))
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
