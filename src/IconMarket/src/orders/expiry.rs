//! Expiring unanswered orders (FR-ORD-05, AC-11), every minute (FR-JOB-02).
//!
//! An order its seller has neither accepted nor rejected within `order.pendingTimeoutMinutes`
//! of being placed becomes `EXPIRED`, gives back its stock, and its buyer and seller are told.
//! Each order expires in a transaction of its own, which also queues its notices, sent once it
//! commits; a push cut off by a crash is sent by the retry job (FR-LINE-12). A run that stops
//! part way is simply run again: an order that expired, or that its seller accepted meanwhile,
//! is no longer `PENDING`, so nothing happens to it twice (FR-JOB-03).

#[cfg(test)]
mod tests;

use chrono::{DateTime, TimeDelta, Utc};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    TransactionTrait,
};

use super::actions::{self, Parties};
use super::lifecycle::{self, By, Moved};
use crate::config::{NotificationConfig, OrderConfig};
use crate::line;
use crate::notifications::templates::Templates;
use crate::notifications::{self, Outbox};
use crate::persistence::entity::order::{self, Status};

/// Expires the orders still `PENDING` `order.pendingTimeoutMinutes` after they were placed, as
/// of `now`, oldest first, and tells their buyers and sellers (Table 4.8-A). Returns how many
/// expired.
pub async fn expire(
    db: &DatabaseConnection,
    line: &line::Client,
    templates: &Templates,
    config: &OrderConfig,
    notification: &NotificationConfig,
    now: DateTime<Utc>,
) -> Result<u64, DbErr> {
    let minutes = config.pending_timeout_minutes;
    let placed_by = now - TimeDelta::minutes(minutes.into());
    let ids: Vec<i64> = order::Entity::find()
        .select_only()
        .column(order::Column::Id)
        .filter(order::Column::Status.eq(Status::Pending))
        .filter(order::Column::CreatedAt.lte(placed_by))
        .order_by_asc(order::Column::CreatedAt)
        .order_by_asc(order::Column::Id)
        .into_tuple()
        .all(db)
        .await?;
    let mut expired = 0;
    for id in ids {
        let txn = db.begin().await?;
        // One its seller answered, or its buyer cancelled, meanwhile stays as it is
        let Moved::Done { order, .. } =
            lifecycle::transition(&txn, id, Status::Expired, By::SYSTEM, None, now).await?
        else {
            continue;
        };
        let parties = Parties::of(&txn, &order).await?;
        let mut outbox = Outbox::default();
        for (kind, to, notice) in actions::expired_notices(templates, &order, &parties, minutes) {
            outbox.add(kind, to, vec![notice]);
        }
        let queued = outbox.queue(&txn, notification, now).await?;
        txn.commit().await?;
        notifications::send(db, line, queued).await;
        expired += 1;
    }
    Ok(expired)
}
