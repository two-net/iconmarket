//! One call to the AI provider, for usage and cost (FR-AI-16).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "ai_call_logs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `None` for calls not made for a user
    pub user_id: Option<i64>,
    pub feature: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub latency_ms: i32,
    pub outcome: String,
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
