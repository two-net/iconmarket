//! Transfer evidence sent by the buyer (FR-PAY-05).

use sea_orm::entity::prelude::*;

use super::PersonalData;
use crate::thai::Money;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "payment_slips")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub order_id: i64,
    /// `None` once retention has deleted the image (§5.4)
    pub image_id: Option<Uuid>,
    /// The random token of the expiring link to the image (NFR-SEC-04)
    #[sea_orm(unique)]
    pub access_token: Uuid,
    pub token_expires_at: DateTimeUtc,
    /// What the AI read from the slip; advisory only (FR-PAY-06)
    pub ai_amount: Option<Money>,
    pub ai_paid_at: Option<DateTimeUtc>,
    pub ai_reference: Option<String>,
    pub status: Status,
    pub created_at: DateTimeUtc,
}

/// The seller's answer: "ได้รับเงินแล้ว" or "ยังไม่ได้รับ" (FR-PAY-05).
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Submitted,
    Confirmed,
    Rejected,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::order::Entity",
        from = "Column::OrderId",
        to = "super::order::Column::Id"
    )]
    Order,
    #[sea_orm(
        belongs_to = "super::image::Entity",
        from = "Column::ImageId",
        to = "super::image::Column::Id",
        on_delete = "SetNull"
    )]
    Image,
}

impl ActiveModelBehavior for ActiveModel {}

impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[Column::ImageId, Column::AiReference];
}
