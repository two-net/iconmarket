//! An admin or sensitive action: who, what, target, when and why (FR-ADM-08).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "audit_logs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `None` for the system
    pub actor_id: Option<i64>,
    pub action: String,
    pub target_type: String,
    /// Not a foreign key, as the target can be in any table
    pub target_id: Option<i64>,
    #[sea_orm(column_type = "JsonBinary")]
    pub details: Json,
    pub created_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::ActorId",
        to = "super::user::Column::Id"
    )]
    Actor,
}

impl ActiveModelBehavior for ActiveModel {}
