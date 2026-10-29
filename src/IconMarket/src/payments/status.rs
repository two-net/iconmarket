//! Payment status (FR-PAY-07 – FR-PAY-09): who may record that an order was paid or refunded,
//! and recording it.
//!
//! IconMarket never holds or moves money (FR-PAY-01): its seller, or an admin, records what
//! passed between the parties. [`allowed`] is FR-PAY-08's rule, and [`record`] the only way a
//! payment status is set by hand: in the caller's transaction, it locks the order as the
//! [lifecycle](crate::orders::lifecycle) does, so recording a payment while the order is
//! cancelled has one winner, and checks the change is allowed from where the order is now. A
//! paid order cancelled is to be refunded, which the lifecycle sets as it cancels it; a transfer
//! awaits its seller's confirmation once the buyer sends its [slip](super::slip).

#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, DatabaseTransaction, DbErr};
use tracing::info;

use crate::orders::lifecycle::{self, By};
use crate::persistence::entity::order::{self, PaymentMethod, PaymentStatus, Status};
use crate::persistence::entity::order_event::ActorType;

/// The payment statuses of an order not paid: none recorded, or a transfer its seller is yet
/// to confirm (FR-PAY-08)
pub const UNPAID: [PaymentStatus; 2] = [PaymentStatus::Unpaid, PaymentStatus::AwaitingConfirmation];

/// Whether `by` may record the payment of an order paid by `method`, now `order`, as `to` rather
/// than `from` (FR-PAY-08): only its seller or an admin, and only that it was paid, once accepted
/// and not called off, or refunded, once owed back. Cash is paid on receipt, so is recorded once
/// the order is handed over (FR-PAY-07); a transfer as soon as it is accepted (FR-PAY-03).
pub fn allowed(
    from: PaymentStatus,
    to: PaymentStatus,
    by: ActorType,
    order: Status,
    method: PaymentMethod,
) -> bool {
    use PaymentStatus::{Paid, RefundDue, Refunded};
    if !matches!(by, ActorType::Seller | ActorType::Admin) {
        return false;
    }
    match (from, to) {
        (from, Paid) if UNPAID.contains(&from) => {
            order == Status::Completed || (order.is_active() && method == PaymentMethod::PromptPay)
        }
        // Settled between the parties (FR-PAY-09)
        (RefundDue, Refunded) => true,
        _ => false,
    }
}

/// What became of recording a payment.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    /// It was `from`, and is now `order`'s
    Done {
        from: PaymentStatus,
        order: order::Model,
    },
    /// It can't be recorded from where the order is now, as it is: it moved meanwhile, or it
    /// never could. Nothing changed.
    Refused(order::Model),
    /// There is no such order
    NotFound,
}

/// Records order `id` as `to` at `now`, by `by`, in `txn`, if [`allowed`] from where the order
/// is now. Nobody records their own order, not even an admin (FR-PAY-08).
pub async fn record(
    txn: &DatabaseTransaction,
    id: i64,
    to: PaymentStatus,
    by: By,
    now: DateTime<Utc>,
) -> Result<Recorded, DbErr> {
    let Some(current) = lifecycle::lock(txn, id).await? else {
        return Ok(Recorded::NotFound);
    };
    let from = current.payment_status;
    let own = by.id == Some(current.buyer_id);
    if own || !allowed(from, to, by.actor, current.status, current.payment_method) {
        return Ok(Recorded::Refused(current));
    }
    let order = order::ActiveModel {
        id: Unchanged(id),
        payment_status: Set(to),
        updated_at: Set(now),
        ..Default::default()
    }
    .update(txn)
    .await?;
    // The slip the seller was asked about is the transfer recorded (FR-PAY-05)
    if to == PaymentStatus::Paid {
        super::slip::confirm(txn, id).await?;
    }
    info!(
        order_id = id,
        from = ?from,
        to = ?to,
        by = ?by.actor,
        "payment recorded"
    );
    Ok(Recorded::Done { from, order })
}
