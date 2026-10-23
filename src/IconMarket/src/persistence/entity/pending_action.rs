//! A change waiting for the user's confirmation, referenced by postback buttons (FR-UI-13).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "pending_actions")]
pub struct Model {
    /// Random, as it travels in postback data
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(indexed)]
    pub user_id: i64,
    pub kind: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
    pub expires_at: DateTimeUtc,
    /// Set when confirmed, so a repeated tap has no further effect
    pub consumed_at: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
}

impl ActiveModelBehavior for ActiveModel {}
