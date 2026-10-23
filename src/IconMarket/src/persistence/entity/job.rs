//! A background job's schedule and last run, one row per job, so jobs resume after a restart
//! (FR-JOB-03) and a run is claimed by one instance at a time (FR-JOB-04).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "jobs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
    /// The schedule `due_at` was computed for, such as `every 900s` or
    /// `daily 03:00 Asia/Bangkok`; a different one at startup reschedules the job
    pub schedule: String,
    /// When the next run is due. Moved on only when a run ends, so a run cut short is run
    /// again.
    pub due_at: DateTimeUtc,
    /// When the last run started
    pub last_run_at: Option<DateTimeUtc>,
    /// Why the last run failed; `None` when it succeeded
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
