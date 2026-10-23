//! A line of an order. Name, unit and unit price are snapshots taken at checkout, so later
//! product edits never change the order (DR-03).

use sea_orm::entity::prelude::*;

use crate::thai::Money;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "order_items")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub order_id: i64,
    pub product_id: i64,
    pub name: String,
    pub unit: String,
    pub unit_price: Money,
    pub quantity: i32,
    pub line_total: Money,
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
        belongs_to = "super::product::Entity",
        from = "Column::ProductId",
        to = "super::product::Column::Id"
    )]
    Product,
}

impl ActiveModelBehavior for ActiveModel {}
