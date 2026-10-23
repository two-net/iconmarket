//! The event inbox: every webhook event, stored before LINE is acknowledged and processed
//! from here (FR-LINE-03 – FR-LINE-05).

use sea_orm::entity::prelude::*;

use super::PersonalData;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "webhook_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// LINE's `webhookEventId`, so a redelivered event is stored once (FR-LINE-04)
    #[sea_orm(unique)]
    pub event_id: String,
    /// The LINE source the event came from; events are processed in order per key
    #[sea_orm(indexed)]
    pub user_key: String,
    pub event_type: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
    pub received_at: DateTimeUtc,
    #[sea_orm(indexed)]
    pub status: Status,
    pub attempts: i32,
    pub processed_at: Option<DateTimeUtc>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Pending,
    Processing,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The key is a LINE user ID, and the payload holds it with the user's messages.
impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[Column::UserKey, Column::Payload];
}
