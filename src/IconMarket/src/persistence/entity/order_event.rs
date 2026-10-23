//! An order's status history: every transition with actor, time and reason (FR-ORD-02).

use sea_orm::entity::prelude::*;

use super::order;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "order_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub order_id: i64,
    /// `None` for the checkout that placed the order
    pub from_status: Option<order::Status>,
    pub to_status: order::Status,
    pub actor_type: ActorType,
    /// `None` for the system
    pub actor_id: Option<i64>,
    pub reason: Option<String>,
    pub created_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum ActorType {
    Buyer,
    Seller,
    Admin,
    System,
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
        belongs_to = "super::user::Entity",
        from = "Column::ActorId",
        to = "super::user::Column::Id"
    )]
    Actor,
}

impl ActiveModelBehavior for ActiveModel {}
