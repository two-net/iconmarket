//! Databases for tests: a throwaway PostgreSQL per test, which needs Docker, or a pool that
//! never connects.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ContainerRequest, ImageExt};

use super::{connect, migrate};
use crate::config::{DatabaseConfig, Secret};

/// An empty database, and the container serving it.
pub async fn start(image: ContainerRequest<Postgres>) -> (ContainerAsync<Postgres>, PgPool) {
    let container = image.with_tag("18").start().await.expect(
        "cannot start PostgreSQL; is Docker running at DOCKER_HOST or /var/run/docker.sock?",
    );
    let pool = pool(&container).await;
    (container, pool)
}

/// Another pool on the database `container` serves, as a second instance of the app has.
pub async fn pool(container: &ContainerAsync<Postgres>) -> PgPool {
    let config = DatabaseConfig {
        host: container.get_host().await.unwrap().to_string(),
        port: container.get_host_port_ipv4(5432).await.unwrap(),
        name: "postgres".to_owned(),
        user: "postgres".to_owned(),
        password: Secret::new("postgres"),
        ssl_mode: PgSslMode::Disable,
        max_connections: 2,
    };
    connect(&config).await.unwrap()
}

/// A fresh database holding the schema, and the container serving it.
pub async fn migrated() -> (ContainerAsync<Postgres>, PgPool) {
    let (container, pool) = start(Postgres::default().into()).await;
    migrate(&pool).await.unwrap();
    (container, pool)
}

/// A pool whose database is always down: nothing listens on port 1.
pub fn unreachable() -> PgPool {
    let options = PgConnectOptions::new_without_pgpass()
        .host("127.0.0.1")
        .port(1);
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy_with(options)
}
