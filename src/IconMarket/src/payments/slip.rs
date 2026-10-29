//! Payment slips (FR-PAY-05): the buyer's evidence of a PromptPay transfer, and the seller's
//! answer to it.
//!
//! The buyer of an accepted order that [awaits a transfer](awaits_transfer) sends the slip:
//! [`submit`] keeps its photo as the slip's image, reachable only through its expiring link
//! (AC-14), and the order awaits its seller's confirmation. "ได้รับเงินแล้ว" records it paid,
//! which [confirms](confirm) the slip; "ยังไม่ได้รับ" refuses it ([`not_received`]), and the order
//! awaits a transfer again. Like [`record`](super::record), each locks the order in the caller's
//! transaction, so a slip sent while the order is cancelled, or answered twice, has one winner.
//!
//! While `ai.features.slipReading` is on, the slip is [read](reading) as it is sent, for the
//! seller to compare: advice kept with it, which changes nothing of the order (FR-PAY-06).

pub mod reading;
#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::prelude::Uuid;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, QueryOrder, TransactionTrait,
};
use tracing::info;

use crate::catalog::images::{self, Owner, SlipLink};
use crate::orders::lifecycle;
use crate::persistence::entity::order::{self, PaymentMethod, PaymentStatus};
use crate::persistence::entity::payment_slip::{self, Status};

use self::reading::Reading;

/// Whether `order` awaits a PromptPay transfer, so its buyer may send the slip (FR-PAY-05,
/// FR-UI-14): accepted and not yet handed over or called off, and neither paid nor awaiting the
/// seller's confirmation of a slip already sent.
pub fn awaits_transfer(order: &order::Model) -> bool {
    order.status.is_active()
        && order.payment_method == PaymentMethod::PromptPay
        && order.payment_status == PaymentStatus::Unpaid
}

/// What became of sending a slip.
#[derive(Clone, Debug, PartialEq)]
pub enum Submitted {
    /// The order now awaits its seller's confirmation of `slip`
    Done {
        order: order::Model,
        slip: payment_slip::Model,
    },
    /// The order no longer awaits a transfer, as it is, or isn't the buyer's. Nothing changed.
    Refused(order::Model),
    /// The photo is no longer there to keep. Nothing changed.
    Gone,
    /// There is no such order
    NotFound,
}

/// Sends `photo`, a draft of buyer `buyer_id`'s, as the slip of order `order_id` at `now`, in
/// `txn`, if the order is theirs and [awaits a transfer](awaits_transfer): the photo becomes the
/// slip's image, reachable through `link`, and the order awaits its seller's confirmation. What
/// the AI read from it, if it did, is kept with it, and changes nothing else (FR-PAY-06).
pub async fn submit(
    txn: &DatabaseTransaction,
    order_id: i64,
    buyer_id: i64,
    photo: Uuid,
    link: SlipLink,
    reading: Option<&Reading>,
    now: DateTime<Utc>,
) -> Result<Submitted, DbErr> {
    let Some(current) = lifecycle::lock(txn, order_id).await? else {
        return Ok(Submitted::NotFound);
    };
    if current.buyer_id != buyer_id || !awaits_transfer(&current) {
        return Ok(Submitted::Refused(current));
    }
    let read = reading.cloned().unwrap_or_default();
    // Undone if the photo is gone, so that no slip is left without one
    let savepoint = txn.begin().await?;
    let slip = payment_slip::ActiveModel {
        order_id: Set(order_id),
        image_id: Set(None),
        access_token: Set(link.token),
        token_expires_at: Set(link.expires_at),
        ai_amount: Set(read.amount),
        ai_paid_at: Set(read.paid_at),
        ai_reference: Set(read.reference),
        status: Set(Status::Submitted),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(&savepoint)
    .await?;
    if !images::adopt(&savepoint, photo, buyer_id, Owner::Slip(slip.id)).await? {
        savepoint.rollback().await?;
        return Ok(Submitted::Gone);
    }
    let slip = payment_slip::ActiveModel {
        id: Unchanged(slip.id),
        image_id: Set(Some(photo)),
        ..Default::default()
    }
    .update(&savepoint)
    .await?;
    let order = order::ActiveModel {
        id: Unchanged(order_id),
        payment_status: Set(PaymentStatus::AwaitingConfirmation),
        updated_at: Set(now),
        ..Default::default()
    }
    .update(&savepoint)
    .await?;
    savepoint.commit().await?;
    info!(order_id, slip_id = slip.id, "payment slip sent");
    Ok(Submitted::Done { order, slip })
}

/// What became of the seller's "ยังไม่ได้รับ".
#[derive(Clone, Debug, PartialEq)]
pub enum Answered {
    /// The slip is refused, and the order, now `order`, awaits a transfer again
    Done(order::Model),
    /// The order no longer awaits the answer, as it is: answered already, recorded paid, or
    /// another slip sent since. Nothing changed.
    Refused(order::Model),
    /// There is no such order
    NotFound,
}

/// The seller's "ยังไม่ได้รับ" on order `order_id` at `now`, in `txn`: on the notice of its slip
/// `slip_id`, or, without one, asked whether it was paid as it was handed over (FR-PAY-07). The
/// slip awaiting their answer is refused, and the order awaits a transfer again. Who may answer
/// is the caller's to check.
pub async fn not_received(
    txn: &DatabaseTransaction,
    order_id: i64,
    slip_id: Option<i64>,
    now: DateTime<Utc>,
) -> Result<Answered, DbErr> {
    let Some(current) = lifecycle::lock(txn, order_id).await? else {
        return Ok(Answered::NotFound);
    };
    if current.payment_status != PaymentStatus::AwaitingConfirmation {
        return Ok(Answered::Refused(current));
    }
    let pending = pending(txn, order_id).await?;
    match (slip_id, pending) {
        (Some(id), Some(slip)) if id != slip.id => return Ok(Answered::Refused(current)),
        (Some(_), None) => return Ok(Answered::Refused(current)),
        (_, Some(slip)) => {
            payment_slip::ActiveModel {
                id: Unchanged(slip.id),
                status: Set(Status::Rejected),
                ..Default::default()
            }
            .update(txn)
            .await?;
        }
        (None, None) => {}
    }
    let order = order::ActiveModel {
        id: Unchanged(order_id),
        payment_status: Set(PaymentStatus::Unpaid),
        updated_at: Set(now),
        ..Default::default()
    }
    .update(txn)
    .await?;
    info!(order_id, "payment slip refused: not received");
    Ok(Answered::Done(order))
}

/// Confirms the slip of order `order_id` awaiting its seller's answer, if any, as the order is
/// recorded paid.
pub(super) async fn confirm(db: &impl ConnectionTrait, order_id: i64) -> Result<(), DbErr> {
    payment_slip::Entity::update_many()
        .col_expr(payment_slip::Column::Status, Expr::value(Status::Confirmed))
        .filter(payment_slip::Column::OrderId.eq(order_id))
        .filter(payment_slip::Column::Status.eq(Status::Submitted))
        .exec(db)
        .await?;
    Ok(())
}

/// The slip of order `order_id` awaiting its seller's answer, if any: the last one sent.
async fn pending(
    db: &impl ConnectionTrait,
    order_id: i64,
) -> Result<Option<payment_slip::Model>, DbErr> {
    payment_slip::Entity::find()
        .filter(payment_slip::Column::OrderId.eq(order_id))
        .filter(payment_slip::Column::Status.eq(Status::Submitted))
        .order_by_desc(payment_slip::Column::Id)
        .one(db)
        .await
}
