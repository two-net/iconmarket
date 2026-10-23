//! Scheduler (SRS §2.1, §4.12): background jobs, run inside the process (FR-JOB-01).
//!
//! Each job has a row in `jobs` holding when its next run is due (FR-JOB-03). An instance
//! claims a run by locking that row, so however many instances there are, each run happens
//! once (FR-JOB-04). The due time moves on only when the run ends. A run cut short by a crash
//! or by shutdown is therefore run again at the next start, and its lock is released with its
//! connection, so nothing waits for it to time out (NFR-REL-03).
//!
//! A job may thus run again after it took effect in part, so every job must be idempotent. It
//! gets the connection pool and commits as it goes; the lock holds a connection of its own.
//!
//! A job is a variant of [`Job`]; its work belongs to the module that owns its data.

#[cfg(test)]
mod tests;

use std::cmp;
use std::future::Future;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::sea_query::{Alias, Expr, ExprTrait, LockBehavior, LockType, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter,
    QuerySelect, TransactionTrait,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::dispatcher;
use crate::persistence::{self, entity::job};

/// How soon a failed run is tried again, unless the job is due sooner anyway.
const RETRY_AFTER: TimeDelta = TimeDelta::minutes(1);

/// The longest a job waits before looking at its row again, so that it notices a run another
/// instance rescheduled, or the system clock being changed.
const MAX_WAIT: TimeDelta = TimeDelta::minutes(1);

/// A background job (FR-JOB-02). The others are added by their issues: the webhook inbox
/// (#14), push retries (#15), retention (#16), pending-order expiry (#33), and the summary and
/// digest (#47, #48).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    /// Deletes expired sessions and pending actions (§5.4)
    DeleteExpired,
}

impl Job {
    const ALL: [Self; 1] = [Self::DeleteExpired];

    /// Its key in `jobs` and in log lines.
    pub fn name(self) -> &'static str {
        match self {
            Self::DeleteExpired => "delete-expired",
        }
    }

    fn schedule(self) -> Schedule {
        match self {
            Self::DeleteExpired => Schedule::Every(TimeDelta::minutes(15)),
        }
    }

    /// Does one run.
    async fn run(self, context: Context) -> anyhow::Result<()> {
        match self {
            Self::DeleteExpired => {
                let deleted = dispatcher::delete_expired(&context.db, Utc::now()).await?;
                debug!(
                    sessions = deleted.sessions,
                    pending_actions = deleted.pending_actions,
                    "expired sessions and pending actions deleted"
                );
            }
        }
        Ok(())
    }
}

/// What jobs work with.
#[derive(Clone)]
struct Context {
    db: DatabaseConnection,
    /// The business time zone (`app.timeZone`), which daily schedules are in
    time_zone: Tz,
}

/// When a job runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Every period, counted from the start of the previous run
    Every(TimeDelta),
    /// Daily at this time in the business time zone. A day the app was down for is not made up
    /// for: an overdue run happens once.
    #[allow(
        dead_code,
        reason = "the retention, summary and digest jobs are built by #16, #47 and #48"
    )]
    Daily(NaiveTime),
}

impl Schedule {
    /// When a newly registered job is first due: a periodic one straight away, a daily one at
    /// its next time.
    fn first(self, now: DateTime<Utc>, time_zone: Tz) -> DateTime<Utc> {
        match self {
            Self::Every(_) => now,
            Self::Daily(_) => self.next(now, time_zone),
        }
    }

    /// When the run after one that started at `now` is due.
    fn next(self, now: DateTime<Utc>, time_zone: Tz) -> DateTime<Utc> {
        match self {
            Self::Every(period) => now + period,
            Self::Daily(time) => {
                let today = now.with_timezone(&time_zone).date_naive();
                let at = local(today, time, time_zone);
                if at > now {
                    at
                } else {
                    let tomorrow = today.succ_opt().expect("a date before the year 262143");
                    local(tomorrow, time, time_zone)
                }
            }
        }
    }

    /// How `jobs.schedule` records it.
    fn describe(self, time_zone: Tz) -> String {
        match self {
            Self::Every(period) => format!("every {}s", period.num_seconds()),
            Self::Daily(time) => format!("daily {} {time_zone}", time.format("%H:%M")),
        }
    }
}

/// The first instant at which the clocks in `time_zone` show `time` on `date`: the earlier one
/// when the clocks go back, and the end of the gap when they skip `time`.
fn local(date: NaiveDate, time: NaiveTime, time_zone: Tz) -> DateTime<Utc> {
    let mut at = date.and_time(time);
    loop {
        if let Some(instant) = time_zone.from_local_datetime(&at).earliest() {
            return instant.to_utc();
        }
        // Clocks change on the minute
        at += TimeDelta::minutes(1);
    }
}

/// Registers the jobs, then runs each on `tracker` whenever it is due. Each job stops picking
/// up work once `stop` is cancelled; a run still going when time runs out is run again at the
/// next start (NFR-REL-05).
pub async fn start(tracker: &TaskTracker, stop: CancellationToken, db: PgPool, time_zone: Tz) {
    let context = Context {
        db: persistence::orm(db),
        time_zone,
    };
    for job in Job::ALL {
        let (name, now) = (job.name(), Utc::now());
        let registered = match register(&context.db, name, job.schedule(), time_zone, now).await {
            Ok(()) => true,
            Err(e) => {
                warn!(job = name, error = %e, "cannot register the job; trying again shortly");
                false
            }
        };
        tracker.spawn(keep_running(job, context.clone(), registered, stop.clone()));
    }
    info!(jobs = Job::ALL.len(), "background jobs started");
}

/// Runs `job` whenever it is due, until `stop` is cancelled.
async fn keep_running(job: Job, context: Context, mut registered: bool, stop: CancellationToken) {
    let (name, schedule) = (job.name(), job.schedule());
    while !stop.is_cancelled() {
        let now = Utc::now();
        let result = if registered {
            tick(
                &context.db,
                name,
                schedule,
                context.time_zone,
                now,
                job.run(context.clone()),
            )
            .await
        } else {
            register(&context.db, name, schedule, context.time_zone, now)
                .await
                .map(|()| Tick::NotDue(now))
        };
        let wait = match result {
            Ok(Tick::Ran(due_at) | Tick::NotDue(due_at)) => {
                registered = true;
                (due_at - Utc::now()).clamp(TimeDelta::zero(), MAX_WAIT)
            }
            Ok(Tick::Busy) => MAX_WAIT,
            Err(e) => {
                warn!(job = name, error = %e, "cannot check the job; trying again shortly");
                // In case its row is gone
                registered = false;
                MAX_WAIT
            }
        };
        tokio::select! {
            () = stop.cancelled() => {}
            () = tokio::time::sleep(wait.to_std().unwrap_or_default()) => {}
        }
    }
}

/// Adds the job `name` to `jobs`. A job already there keeps its due time, so it resumes where
/// it was, unless its schedule changed: then it is due at the new schedule's first time.
async fn register(
    db: &DatabaseConnection,
    name: &str,
    schedule: Schedule,
    time_zone: Tz,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    let row = job::ActiveModel {
        name: Set(name.to_owned()),
        schedule: Set(schedule.describe(time_zone)),
        due_at: Set(schedule.first(now, time_zone)),
        last_run_at: Set(None),
        last_error: Set(None),
    };
    let stored = Expr::col((job::Entity, job::Column::Schedule));
    let registering = Expr::col((Alias::new("excluded"), job::Column::Schedule));
    job::Entity::insert(row)
        .on_conflict(
            OnConflict::column(job::Column::Name)
                .update_columns([job::Column::Schedule, job::Column::DueAt])
                .action_and_where(stored.ne(registering))
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    Ok(())
}

/// What [`tick`] found.
#[derive(Debug, PartialEq, Eq)]
enum Tick {
    /// It ran, well or not, and the next run is due then
    Ran(DateTime<Utc>),
    /// It is not due until then
    NotDue(DateTime<Utc>),
    /// Another instance is running it
    Busy,
}

/// Runs the job `name` with `work` if it is due at `now` and no other instance is running it.
/// `work` runs while the job's row is locked, and the next due time is stored when it ends: a
/// failed run is tried again after [`RETRY_AFTER`].
async fn tick(
    db: &DatabaseConnection,
    name: &str,
    schedule: Schedule,
    time_zone: Tz,
    now: DateTime<Utc>,
    work: impl Future<Output = anyhow::Result<()>> + Send + 'static,
) -> Result<Tick, DbErr> {
    let claim = db.begin().await?;
    let due = job::Entity::find_by_id(name)
        .filter(job::Column::DueAt.lte(now))
        .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
        .one(&claim)
        .await?;
    if due.is_none() {
        claim.rollback().await?;
        // Without a lock, so that a row another instance holds can be read
        let row = job::Entity::find_by_id(name)
            .one(db)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("job {name}")))?;
        return Ok(if row.due_at > now {
            Tick::NotDue(row.due_at)
        } else {
            Tick::Busy
        });
    }

    // On a task of its own, so that a panic fails the run instead of ending the job. Its log
    // lines carry the job's name.
    let result = match tokio::spawn(work.instrument(info_span!("job", name))).await {
        Ok(result) => result,
        Err(e) => Err(e.into()),
    };
    let next = schedule.next(now, time_zone);
    let (due_at, last_error) = match &result {
        Ok(()) => (next, None),
        Err(e) => (cmp::min(next, now + RETRY_AFTER), Some(format!("{e:#}"))),
    };
    let ran = job::ActiveModel {
        name: Unchanged(name.to_owned()),
        due_at: Set(due_at),
        last_run_at: Set(Some(now)),
        last_error: Set(last_error.clone()),
        ..Default::default()
    };
    ran.update(&claim).await?;
    claim.commit().await?;

    match last_error {
        None => debug!(job = name, next_at = %due_at, "job run finished"),
        Some(error) => warn!(job = name, error, retry_at = %due_at, "job run failed"),
    }
    Ok(Tick::Ran(due_at))
}
