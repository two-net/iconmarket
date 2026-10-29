//! An item for sale (FR-PROD). Its photos are images owned by the product.

use sea_orm::entity::prelude::*;

use crate::thai::Money;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "products")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub shop_id: i64,
    #[sea_orm(indexed)]
    pub category_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub price: Money,
    pub unit: String,
    /// The quantity still available for new orders (FR-PROD-05); `None` is unlimited or made
    /// to order. The database refuses a negative value (DR-07).
    #[sea_orm(extra = "CONSTRAINT products_stock_not_negative CHECK (stock >= 0)")]
    pub stock: Option<i32>,
    #[sea_orm(indexed)]
    pub status: Status,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    /// When its seller last made more of it available for new orders: with `created_at`, what
    /// "ของใหม่วันนี้" lists it by (FR-SRCH-09)
    pub restocked_at: Option<DateTimeUtc>,
    /// Set together with `DELETED`; products are never removed (DR-04)
    pub deleted_at: Option<DateTimeUtc>,
    /// The day a pre-order ("สั่งล่วงหน้า") is ready, in the business time zone (FR-PROD-11).
    /// It is a pre-order only while that day is still to come; from then on, an ordinary product.
    pub ready_on: Option<Date>,
}

/// FR-PROD-07. Sold out is derived from the stock, not a status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Draft,
    Active,
    Hidden,
    /// Under review (FR-PROD-09)
    Held,
    Removed,
    Deleted,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::shop::Entity",
        from = "Column::ShopId",
        to = "super::shop::Column::Id"
    )]
    Shop,
    #[sea_orm(
        belongs_to = "super::category::Entity",
        from = "Column::CategoryId",
        to = "super::category::Column::Id"
    )]
    Category,
}

impl ActiveModelBehavior for ActiveModel {}
