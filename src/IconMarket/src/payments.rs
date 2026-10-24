//! Payments (SRS §2.1): payment method and status, PromptPay details and slips.
//! Built by #35, #40 and #41.

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};

use crate::persistence::entity::order::{self, PaymentMethod, PaymentStatus, Status};

/// Buyer `buyer_id`'s latest order that its seller accepted and that awaits a PromptPay
/// transfer: the order a photo the buyer sends may be the slip of (FR-UI-14, FR-PAY-05).
pub async fn awaiting_transfer(
    db: &impl ConnectionTrait,
    buyer_id: i64,
) -> Result<Option<order::Model>, DbErr> {
    order::Entity::find()
        .filter(order::Column::BuyerId.eq(buyer_id))
        .filter(order::Column::Status.is_in([Status::Accepted, Status::Ready, Status::Delivering]))
        .filter(order::Column::PaymentMethod.eq(PaymentMethod::PromptPay))
        .filter(order::Column::PaymentStatus.eq(PaymentStatus::Unpaid))
        .order_by_desc(order::Column::CreatedAt)
        .one(db)
        .await
}
