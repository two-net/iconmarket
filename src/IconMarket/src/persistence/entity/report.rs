//! A problem report in the admin review queue (FR-HELP-02, FR-ADM-03).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "reports")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `None` when raised by moderation
    pub reporter_id: Option<i64>,
    pub target_type: TargetType,
    /// The product or shop; not a foreign key, as it points at one of two tables
    pub target_id: i64,
    pub reason: String,
    pub status: Status,
    pub resolved_by_id: Option<i64>,
    pub resolved_at: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum TargetType {
    Product,
    Shop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Open,
    Resolved,
    Dismissed,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::ReporterId",
        to = "super::user::Column::Id"
    )]
    Reporter,
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::ResolvedById",
        to = "super::user::Column::Id"
    )]
    ResolvedBy,
}

impl ActiveModelBehavior for ActiveModel {}
