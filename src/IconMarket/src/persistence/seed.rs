//! Seed data, applied idempotently at every startup (MIG-05): the default categories and the
//! admin role.

use std::collections::HashSet;

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Condition, Expr};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, PaginatorTrait, QueryFilter,
    TransactionTrait,
};
use sqlx::PgPool;

use super::entity::{category, user};
use super::migration::lock;
use super::orm;
use crate::config::{AdminConfig, CatalogConfig};

/// What seeding changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Seeded {
    /// Default categories created
    pub categories: u64,
    /// Users who became admins
    pub granted: u64,
    /// Users who stopped being admins
    pub revoked: u64,
}

/// Creates the default categories while there are none, and gives the admin role to exactly
/// the users listed in the config (FR-ADM-01). Runs under the migration lock, so instances
/// starting together seed once (MIG-07).
pub async fn seed(
    db: &PgPool,
    catalog: &CatalogConfig,
    admin: &AdminConfig,
) -> anyhow::Result<Seeded> {
    let txn = orm(db.clone()).begin().await?;
    lock(&txn).await?;
    let categories = seed_categories(&txn, &catalog.default_categories).await?;
    let (granted, revoked) = sync_admins(&txn, &admin.line_user_ids).await?;
    txn.commit().await?;
    Ok(Seeded {
        categories,
        granted,
        revoked,
    })
}

/// Creates `names` as categories, in that order, unless a category exists: admins manage them
/// after the first start (FR-ADM-05).
async fn seed_categories(txn: &DatabaseTransaction, names: &[String]) -> Result<u64, DbErr> {
    if category::Entity::find().count(txn).await? > 0 {
        return Ok(0);
    }
    // Names are unique, so a repeat in the config is skipped
    let mut seen = HashSet::new();
    let categories: Vec<_> = names
        .iter()
        .filter(|name| seen.insert(name.as_str()))
        .zip(1..)
        .map(|(name, position)| category::ActiveModel {
            name: Set(name.clone()),
            sort_order: Set(position),
            is_active: Set(true),
            ..Default::default()
        })
        .collect();
    let count = categories.len() as u64;
    if count > 0 {
        category::Entity::insert_many(categories).exec(txn).await?;
    }
    Ok(count)
}

/// Grants the admin role to the users in `line_user_ids` and removes it from everyone else,
/// deleted accounts included. Returns how many were granted and revoked.
///
/// Listed users who haven't followed the OA have no row yet; they get the role when they do.
async fn sync_admins(
    txn: &DatabaseTransaction,
    line_user_ids: &[String],
) -> Result<(u64, u64), DbErr> {
    let now = Utc::now();
    let granted = user::Entity::update_many()
        .col_expr(user::Column::IsAdmin, Expr::value(true))
        .col_expr(user::Column::UpdatedAt, Expr::value(now))
        .filter(user::Column::IsAdmin.eq(false))
        .filter(user::Column::LineUserId.is_in(line_user_ids))
        .exec(txn)
        .await?
        .rows_affected;
    let revoked = user::Entity::update_many()
        .col_expr(user::Column::IsAdmin, Expr::value(false))
        .col_expr(user::Column::UpdatedAt, Expr::value(now))
        .filter(user::Column::IsAdmin.eq(true))
        .filter(
            Condition::any()
                .add(user::Column::LineUserId.is_null())
                .add(user::Column::LineUserId.is_not_in(line_user_ids)),
        )
        .exec(txn)
        .await?
        .rows_affected;
    Ok((granted, revoked))
}
