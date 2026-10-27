//! One checkout at one shop (FR-CART-04, FR-ORD).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use super::PersonalData;
use crate::thai::Money;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "orders")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `IM-YYYYMMDD-NNNN` (FR-ORD-01)
    #[sea_orm(unique)]
    pub order_number: String,
    /// The pending action confirmed at checkout, so a repeated tap places no second order
    /// (FR-UI-13). Not a foreign key: pending actions are deleted once they expire.
    #[sea_orm(unique)]
    pub checkout_key: Uuid,
    #[sea_orm(indexed)]
    pub buyer_id: i64,
    #[sea_orm(indexed)]
    pub shop_id: i64,
    #[sea_orm(indexed)]
    pub status: Status,
    pub fulfillment: Fulfillment,
    /// The delivery address at checkout; `None` for pickup
    pub delivery_zone: Option<String>,
    pub delivery_house_number: Option<String>,
    pub delivery_landmark_note: Option<String>,
    pub delivery_latitude: Option<f64>,
    pub delivery_longitude: Option<f64>,
    pub note: Option<String>,
    pub subtotal: Money,
    pub delivery_fee: Money,
    pub total: Money,
    pub payment_method: PaymentMethod,
    pub payment_status: PaymentStatus,
    /// When the order was placed
    #[sea_orm(indexed)]
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub accepted_at: Option<DateTimeUtc>,
    pub ready_at: Option<DateTimeUtc>,
    pub delivering_at: Option<DateTimeUtc>,
    pub completed_at: Option<DateTimeUtc>,
    pub rejected_at: Option<DateTimeUtc>,
    pub cancelled_at: Option<DateTimeUtc>,
    pub expired_at: Option<DateTimeUtc>,
    /// Why the seller rejected it or it was cancelled (FR-ORD-04)
    pub cancel_reason: Option<String>,
}

/// Table 4.6-A
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Pending,
    Accepted,
    Ready,
    Delivering,
    Completed,
    Rejected,
    Cancelled,
    Expired,
}

/// Kept in checkout's flow and confirmation too
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Fulfillment {
    Pickup,
    Delivery,
}

/// FR-PAY-02. Kept in checkout's flow and confirmation too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum PaymentMethod {
    Cash,
    #[sea_orm(string_value = "PROMPTPAY")]
    PromptPay,
}

/// FR-PAY-08
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum PaymentStatus {
    Unpaid,
    AwaitingConfirmation,
    Paid,
    RefundDue,
    Refunded,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::BuyerId",
        to = "super::user::Column::Id"
    )]
    Buyer,
    #[sea_orm(
        belongs_to = "super::shop::Entity",
        from = "Column::ShopId",
        to = "super::shop::Column::Id"
    )]
    Shop,
}

#[async_trait::async_trait]
impl ActiveModelBehavior for ActiveModel {
    /// Orders are kept for good and only anonymized (DR-04, FR-ONB-07).
    async fn before_delete<C>(self, _db: &C) -> Result<Self, DbErr>
    where
        C: ConnectionTrait,
    {
        Err(DbErr::Custom(
            "orders are never deleted, only anonymized (DR-04)".to_owned(),
        ))
    }
}

impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[
        Column::DeliveryZone,
        Column::DeliveryHouseNumber,
        Column::DeliveryLandmarkNote,
        Column::DeliveryLatitude,
        Column::DeliveryLongitude,
    ];
}
