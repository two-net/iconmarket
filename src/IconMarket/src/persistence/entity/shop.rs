//! A seller's shop (FR-SHOP). Its photo is an image owned by the shop.

use sea_orm::entity::prelude::*;

use super::PersonalData;
use crate::thai::Money;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "shops")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// A user owns at most one shop (FR-SHOP-02)
    #[sea_orm(unique)]
    pub owner_id: i64,
    pub name: String,
    pub description: String,
    pub status: Status,
    pub is_open: bool,
    /// Free text such as *ทุกวัน 06:00–10:00* (FR-SHOP-07)
    pub opening_hours: Option<String>,
    pub offers_pickup: bool,
    pub offers_delivery: bool,
    pub delivery_fee: Money,
    pub minimum_order: Option<Money>,
    pub promptpay_id: Option<String>,
    pub phone: Option<String>,
    /// Whether buyers with an active order see the phone (FR-SHOP-08)
    pub show_phone: bool,
    pub pickup_address: Option<String>,
    pub pickup_latitude: Option<f64>,
    pub pickup_longitude: Option<f64>,
    /// Why an admin turned the application down, while `REJECTED` (FR-SHOP-04); cleared when
    /// the owner applies again
    pub rejection_reason: Option<String>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    /// When the shop became `CLOSED`; shops are never removed (DR-04)
    pub deleted_at: Option<DateTimeUtc>,
}

/// FR-SHOP-10
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Pending,
    Approved,
    Rejected,
    /// Under review once moderation found its edited name, description or photo suspicious
    /// (FR-PROD-09): its owner still runs it, but buyers don't see it and it takes no new orders
    Held,
    Suspended,
    /// Closed for good by the owner or by account deletion
    Closed,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::OwnerId",
        to = "super::user::Column::Id"
    )]
    Owner,
}

impl ActiveModelBehavior for ActiveModel {}

impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[
        Column::PromptpayId,
        Column::Phone,
        Column::PickupAddress,
        Column::PickupLatitude,
        Column::PickupLongitude,
    ];
}
