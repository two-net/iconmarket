//! An outgoing push request and its result (FR-NOT-05). Written as `PENDING` in the transaction
//! of the step that produced it and sent once that commits, so it also serves as the outbox the
//! retry job works from (FR-JOB-02).

use sea_orm::entity::prelude::*;

use super::PersonalData;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "push_logs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// The kinds of notification the request holds, comma-separated
    pub push_type: String,
    /// The LINE user or group ID pushed to
    pub recipient: String,
    /// The `X-Line-Retry-Key`, so a retried push is delivered once
    pub retry_key: Option<Uuid>,
    #[sea_orm(indexed)]
    pub status: Status,
    pub error: Option<String>,
    /// When it was last tried; until then, when it was queued
    pub sent_at: DateTimeUtc,
    /// The request's messages as LINE takes them, kept while it may be sent again and cleared
    /// once it is `SENT` or `FAILED`
    #[sea_orm(column_type = "JsonBinary")]
    pub messages: Option<Json>,
    /// Send calls made so far, each with LINE's own retries
    pub attempts: i32,
    /// When the step that produced it committed; LINE remembers a retry key for 24 hours
    pub queued_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    /// Not sent yet, or to be tried again
    Pending,
    Sent,
    /// Given up
    Failed,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The recipient is a LINE user ID, and the messages can hold names and addresses.
impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[Column::Recipient, Column::Messages];
}
