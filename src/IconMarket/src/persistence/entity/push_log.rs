//! An outgoing push request and its result (FR-NOT-05).

use sea_orm::entity::prelude::*;

use super::PersonalData;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "push_logs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub push_type: String,
    /// The LINE user or group ID pushed to
    pub recipient: String,
    /// The `X-Line-Retry-Key`, so a retried push is delivered once
    pub retry_key: Option<Uuid>,
    pub status: Status,
    pub error: Option<String>,
    pub sent_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Sent,
    Failed,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[Column::Recipient];
}
