//! A LINE group the OA was added to or removed from, one row per group, so the village group
//! digest is posted only while the OA is in the group, and once a day (FR-NOT-07).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "line_groups")]
pub struct Model {
    /// LINE's group ID: C followed by 32 lowercase hex digits
    #[sea_orm(primary_key, auto_increment = false)]
    pub group_id: String,
    /// When the OA was last added to the group, as its `join` event told
    pub joined_at: Option<DateTimeUtc>,
    /// When the OA was last removed from the group, as its `leave` event told; cleared when it
    /// is added again
    pub left_at: Option<DateTimeUtc>,
    /// The business day whose digest was last queued for the group, so that a run of the job
    /// done again posts no second one (FR-JOB-03)
    pub digest_sent_on: Option<Date>,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
