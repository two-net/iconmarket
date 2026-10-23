//! Auto-migration (§5.3): brings the schema up to date with the entity models at startup.
//!
//! SeaORM's schema sync generates the changes from the models (MIG-01). It runs in one
//! transaction, which PostgreSQL makes cover the DDL too, so a failure leaves the schema as it
//! was. Sync is mostly additive, but it would drop a unique key the models no longer declare
//! and can rename a column, so the catalog is compared before and after, and a migration that
//! lost anything is rolled back (MIG-03).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::bail;
use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Alias, ColumnDef, Table};
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseTransaction, DbBackend, DbErr, EntityTrait,
    IdenStatic, Iterable, QueryOrder, RuntimeErr, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::info;

use super::entity::schema_version;
use super::{FromEntities, add_entities, orm, schema};

/// Held while migrating or seeding, so instances starting together take turns (MIG-07). Any
/// fixed number will do; this one spells "ICONMKT".
const LOCK: i64 = 0x0049_434F_4E4D_4B54;

/// Every table, column, constraint and index in the current schema, as (kind, table, name).
///
/// NOT NULL constraints are left out: PostgreSQL 18 lists them, and making a removed
/// property's column nullable drops one.
const CATALOG: &str = "
    SELECT 'table', c.relname::text, ''
    FROM pg_class c
    WHERE c.relnamespace = to_regnamespace(current_schema()) AND c.relkind IN ('r', 'p')
    UNION ALL
    SELECT 'column', c.relname::text, a.attname::text
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
    WHERE c.relnamespace = to_regnamespace(current_schema()) AND c.relkind IN ('r', 'p')
        AND a.attnum > 0 AND NOT a.attisdropped
    UNION ALL
    SELECT 'constraint', c.relname::text, k.conname::text
    FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid
    WHERE c.relnamespace = to_regnamespace(current_schema()) AND k.contype <> 'n'
    UNION ALL
    SELECT 'index', c.relname::text, i.relname::text
    FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_class c ON c.oid = x.indrelid
    WHERE c.relnamespace = to_regnamespace(current_schema())";

/// The NOT NULL columns of every table in the current schema, as (table, column).
const NOT_NULL_COLUMNS: &str = "
    SELECT c.relname::text, a.attname::text
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
    WHERE c.relnamespace = to_regnamespace(current_schema()) AND c.relkind IN ('r', 'p')
        AND a.attnum > 0 AND NOT a.attisdropped AND a.attnotnull
    ORDER BY c.relname, a.attnum";

/// What auto-migration did.
#[derive(Debug)]
pub struct Migration {
    /// The model snapshot now applied (MIG-09)
    pub snapshot: String,
    /// What was created, such as `table users` or `column users.zone`; a new table's columns,
    /// constraints and indexes come with it and aren't listed
    pub added: Vec<String>,
    /// Columns that no model has any more, as `table.column`, made nullable (MIG-03)
    pub relaxed: Vec<String>,
}

/// Brings the schema up to date with the entity models (MIG-02) and records the snapshot
/// applied (MIG-09). Only adds: fails, changing nothing, if that would drop or rename anything
/// (MIG-03).
pub async fn migrate(db: &PgPool) -> anyhow::Result<Migration> {
    match apply(db).await {
        Err(e) if denied(&e) => Err(e.context(
            "the database user needs rights to create and alter objects in its schema (MIG-08)",
        )),
        result => result,
    }
}

async fn apply(db: &PgPool) -> anyhow::Result<Migration> {
    let txn = orm(db.clone()).begin().await?;
    lock(&txn).await?;

    let before = catalog(&txn).await?;
    schema().sync(&txn).await?;
    let relaxed = relax_removed_columns(&txn).await?;
    let after = catalog(&txn).await?;

    let lost: Vec<_> = before.difference(&after).map(ToString::to_string).collect();
    if !lost.is_empty() {
        // Dropping the transaction rolls it back
        bail!(
            "the models would drop or rename {}, but auto-migration only adds (MIG-03)",
            lost.join(", ")
        );
    }

    let snapshot = snapshot();
    record(&txn, &snapshot).await?;
    txn.commit().await?;

    let new_tables: BTreeSet<_> = after
        .difference(&before)
        .filter(|object| object.kind == "table")
        .map(|object| &object.table)
        .collect();
    let added = after
        .difference(&before)
        .filter(|object| object.kind == "table" || !new_tables.contains(&object.table))
        .map(ToString::to_string)
        .collect();
    Ok(Migration {
        snapshot,
        added,
        relaxed,
    })
}

/// Takes the lock that migration and seeding hold until `txn` ends (MIG-07), waiting while
/// another instance holds it.
pub(super) async fn lock(txn: &DatabaseTransaction) -> Result<(), DbErr> {
    let try_lock = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_try_advisory_xact_lock($1)",
        [LOCK.into()],
    );
    let row = txn.query_one_raw(try_lock).await?;
    if row.map(|row| row.try_get_by_index::<bool>(0)).transpose()? == Some(true) {
        return Ok(());
    }
    info!("another instance is migrating the database; waiting");
    let wait = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [LOCK.into()],
    );
    txn.execute_raw(wait).await?;
    Ok(())
}

/// Something in the schema.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Object {
    kind: String,
    table: String,
    /// Empty for a table
    name: String,
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.name.is_empty() {
            write!(f, "{} {}", self.kind, self.table)
        } else {
            write!(f, "{} {}.{}", self.kind, self.table, self.name)
        }
    }
}

async fn catalog(txn: &DatabaseTransaction) -> Result<BTreeSet<Object>, DbErr> {
    let query = Statement::from_string(DbBackend::Postgres, CATALOG);
    txn.query_all_raw(query)
        .await?
        .into_iter()
        .map(|row| {
            Ok(Object {
                kind: row.try_get_by_index(0)?,
                table: row.try_get_by_index(1)?,
                name: row.try_get_by_index(2)?,
            })
        })
        .collect()
}

/// The columns of each model's table.
#[derive(Default)]
struct ModelColumns(BTreeMap<&'static str, Vec<&'static str>>);

impl FromEntities for ModelColumns {
    fn add<E: EntityTrait>(mut self, entity: E) -> Self {
        let columns = E::Column::iter().map(|column| column.as_str()).collect();
        self.0.insert(entity.table_name(), columns);
        self
    }
}

/// Makes the NOT NULL columns that no model has any more nullable, so inserts through the
/// models keep working while the column stays in place (MIG-03). Returns them as
/// `table.column`.
async fn relax_removed_columns(txn: &DatabaseTransaction) -> Result<Vec<String>, DbErr> {
    let models = add_entities(ModelColumns::default()).0;
    let query = Statement::from_string(DbBackend::Postgres, NOT_NULL_COLUMNS);
    let mut relaxed = Vec::new();
    for row in txn.query_all_raw(query).await? {
        let table: String = row.try_get_by_index(0)?;
        let column: String = row.try_get_by_index(1)?;
        // Tables that aren't the models' are left alone
        let Some(columns) = models.get(table.as_str()) else {
            continue;
        };
        if columns.contains(&column.as_str()) {
            continue;
        }
        let alter = Table::alter()
            .table(Alias::new(&table))
            .modify_column(ColumnDef::new(Alias::new(&column)).null())
            .to_owned();
        txn.execute(&alter).await?;
        relaxed.push(format!("{table}.{column}"));
    }
    Ok(relaxed)
}

/// The model snapshot: SHA-256 of the DDL the models generate, in hex (MIG-09).
fn snapshot() -> String {
    let ddl = format!("{:?}", schema());
    Sha256::digest(ddl)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Records `snapshot` unless it is the one applied last.
async fn record(txn: &DatabaseTransaction, snapshot: &str) -> Result<(), DbErr> {
    let latest = schema_version::Entity::find()
        .order_by_desc(schema_version::Column::Id)
        .one(txn)
        .await?;
    if latest.is_some_and(|version| version.snapshot == snapshot) {
        return Ok(());
    }
    schema_version::ActiveModel {
        snapshot: Set(snapshot.to_owned()),
        app_version: Set(env!("CARGO_PKG_VERSION").to_owned()),
        applied_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(())
}

/// Whether `e` is PostgreSQL refusing for lack of privileges (SQLSTATE 42501).
fn denied(e: &anyhow::Error) -> bool {
    let Some(
        DbErr::Exec(RuntimeErr::SqlxError(e))
        | DbErr::Query(RuntimeErr::SqlxError(e))
        | DbErr::Conn(RuntimeErr::SqlxError(e)),
    ) = e.downcast_ref::<DbErr>()
    else {
        return false;
    };
    e.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "42501")
}
