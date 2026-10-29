//! A stored image: a product or shop photo, a payment slip (FR-PROD-04), or the PromptPay QR
//! code of an order (FR-PAY-04). The bytes live in PostgreSQL, the only data store (CON-03).

use sea_orm::entity::prelude::*;

use super::PersonalData;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "images")]
pub struct Model {
    /// Random, as it is part of the public link (NFR-SEC-04)
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub owner_type: OwnerType,
    /// The product, shop, payment slip or order, or the user who sent a draft; not a foreign
    /// key, as it points at one of several tables
    #[sea_orm(indexed)]
    pub owner_id: i64,
    /// Display order among the owner's images
    pub position: i16,
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub size_bytes: i32,
    pub data: Vec<u8>,
    pub created_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum OwnerType {
    Product,
    Shop,
    Slip,
    /// A photo sent in a guided flow before what it is for exists, such as a shop applied for:
    /// owned by the user who sent it until the flow gives it to what it makes, and deleted if
    /// the flow ends without
    Draft,
    /// The PromptPay QR code for an order's exact amount, sent to its buyer as it is accepted
    /// (FR-PAY-04), and deleted once the order is final
    PaymentQr,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// A slip image shows the payer's name and account, and a PromptPay QR code the shop's
/// PromptPay ID.
impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[Column::Data];
}
