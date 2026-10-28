//! The order lifecycle (Table 4.6-B, FR-ORD-02): the only moves an order's status can make, who
//! can make each, and what each does.
//!
//! [`allowed`] is the table itself, and [`transition`] the only way an order moves: in the
//! caller's transaction, it locks the order, checks the move is allowed from where the order is
//! now, and applies it with its effects, recorded in the order's history with actor, time and
//! reason. Of moves made at the same moment, such as the buyer cancelling while the seller
//! accepts, the first to lock the order wins, and the others find it moved and are refused, so
//! each actor sees its status as it is (FR-ORD-02).
//!
//! An order that will never be handed over, rejected, cancelled or expired, gives back the stock
//! it reserved at checkout. The order is locked before its products, which checkout locks in ID
//! order too (FR-CART-06), so the two never wait for each other in a circle.

#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use tracing::info;

use crate::persistence::entity::order::{self, Fulfillment, PaymentStatus, Status};
use crate::persistence::entity::order_event::{self, ActorType};
use crate::persistence::entity::{order_item, product};

/// Whether `by` may move an order received by `fulfillment` from `from` to `to` (Table 4.6-B).
/// Placing it, `PENDING` from nothing, is checkout's.
pub fn allowed(from: Status, to: Status, by: ActorType, fulfillment: Fulfillment) -> bool {
    use ActorType::{Admin, Buyer, Seller, System};
    use Status::{Accepted, Cancelled, Completed, Delivering, Expired, Pending, Ready, Rejected};
    match (from, to) {
        (Pending, Accepted | Rejected) => by == Seller,
        (Pending, Cancelled) => matches!(by, Buyer | Admin),
        // After `order.pendingTimeoutMinutes` (FR-ORD-05)
        (Pending, Expired) => by == System,
        (Accepted, Ready) => by == Seller && fulfillment == Fulfillment::Pickup,
        (Accepted, Delivering) => by == Seller && fulfillment == Fulfillment::Delivery,
        (Accepted, Completed) => by == Seller,
        // The buyer's "ได้รับของแล้ว" (FR-ORD-09)
        (Ready | Delivering, Completed) => matches!(by, Seller | Buyer),
        (Accepted | Ready | Delivering, Cancelled) => matches!(by, Seller | Admin),
        _ => false,
    }
}

/// Whether `by` must give a reason to move an order to `to` (FR-ORD-04): to reject it, and to
/// cancel it as its seller or an admin.
pub fn needs_reason(to: Status, by: ActorType) -> bool {
    match to {
        Status::Rejected => true,
        Status::Cancelled => matches!(by, ActorType::Seller | ActorType::Admin),
        _ => false,
    }
}

/// Whether an order that reaches `to` gives back the stock it reserved: it will never be handed
/// over.
fn releases_stock(to: Status) -> bool {
    matches!(to, Status::Rejected | Status::Cancelled | Status::Expired)
}

/// Who moves an order, as its history records them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct By {
    pub actor: ActorType,
    /// `None` for the system
    pub id: Option<i64>,
}

impl By {
    pub const SYSTEM: Self = Self {
        actor: ActorType::System,
        id: None,
    };

    /// User `id`, acting as `actor`.
    pub fn user(actor: ActorType, id: i64) -> Self {
        Self {
            actor,
            id: Some(id),
        }
    }
}

/// What became of a move.
#[derive(Clone, Debug, PartialEq)]
pub enum Moved {
    /// It moved from `from`, and is now `order`
    Done { from: Status, order: order::Model },
    /// The move isn't allowed from where the order is now, as it is: it moved meanwhile, or it
    /// never could. Nothing changed.
    Refused(order::Model),
    /// There is no such order
    NotFound,
}

/// Order `id`, locked until `txn` ends, so that nothing else moves it meanwhile.
pub async fn lock(txn: &DatabaseTransaction, id: i64) -> Result<Option<order::Model>, DbErr> {
    order::Entity::find_by_id(id)
        .lock_exclusive()
        .one(txn)
        .await
}

/// Moves order `id` to `to` at `now`, by `by`, for `reason`, in `txn`, if Table 4.6-B allows it
/// from where the order is now and the reason is given exactly when one is needed (FR-ORD-04).
/// It is stamped with the time it got there, the reason kept if it is rejected or cancelled, a
/// paid order cancelled is to be refunded (FR-PAY-09), the move is recorded in its history
/// (FR-ORD-02), and the stock it reserved is given back if it will never be handed over.
pub async fn transition(
    txn: &DatabaseTransaction,
    id: i64,
    to: Status,
    by: By,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Moved, DbErr> {
    let Some(current) = lock(txn, id).await? else {
        return Ok(Moved::NotFound);
    };
    let from = current.status;
    if !allowed(from, to, by.actor, current.fulfillment)
        || needs_reason(to, by.actor) != reason.is_some()
    {
        return Ok(Moved::Refused(current));
    }
    let mut row = order::ActiveModel {
        id: Unchanged(id),
        status: Set(to),
        updated_at: Set(now),
        ..Default::default()
    };
    let reached = Set(Some(now));
    match to {
        Status::Pending => unreachable!("no order moves back to PENDING"),
        Status::Accepted => row.accepted_at = reached,
        Status::Ready => row.ready_at = reached,
        Status::Delivering => row.delivering_at = reached,
        Status::Completed => row.completed_at = reached,
        Status::Rejected => row.rejected_at = reached,
        Status::Cancelled => row.cancelled_at = reached,
        Status::Expired => row.expired_at = reached,
    }
    if matches!(to, Status::Rejected | Status::Cancelled) {
        row.cancel_reason = Set(reason.map(str::to_owned));
    }
    if to == Status::Cancelled && current.payment_status == PaymentStatus::Paid {
        row.payment_status = Set(PaymentStatus::RefundDue);
    }
    let order = row.update(txn).await?;
    order_event::ActiveModel {
        order_id: Set(id),
        from_status: Set(Some(from)),
        to_status: Set(to),
        actor_type: Set(by.actor),
        actor_id: Set(by.id),
        reason: Set(reason.map(str::to_owned)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    if releases_stock(to) {
        release_stock(txn, id).await?;
    }
    // Not the reason: it can name people (NFR-PRIV-05)
    info!(
        order_id = id,
        from = ?from,
        to = ?to,
        by = ?by.actor,
        "order moved"
    );
    Ok(Moved::Done { from, order })
}

/// Gives back the stock order `id` reserved at checkout, product by product in ID order, as
/// checkout takes it. Products of unlimited stock took none.
async fn release_stock(txn: &DatabaseTransaction, id: i64) -> Result<(), DbErr> {
    let items = order_item::Entity::find()
        .filter(order_item::Column::OrderId.eq(id))
        .order_by_asc(order_item::Column::ProductId)
        .all(txn)
        .await?;
    for item in items {
        let stock = Expr::col(product::Column::Stock);
        product::Entity::update_many()
            .col_expr(product::Column::Stock, stock.add(item.quantity))
            .filter(product::Column::Id.eq(item.product_id))
            .filter(product::Column::Stock.is_not_null())
            .exec(txn)
            .await?;
    }
    Ok(())
}
