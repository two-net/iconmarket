//! Schedules in the business time zone, and runs against a migrated database: each due run
//! happens once with two instances (FR-JOB-04), a run cut short is run again after a restart
//! (FR-JOB-03, NFR-REL-03), and failed runs are retried. The database tests each start a
//! throwaway PostgreSQL, so they need Docker.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use chrono_tz::America::New_York;
use chrono_tz::Asia::Bangkok;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::*;
use crate::persistence::{orm, testing};

const NAME: &str = "probe";

const EVERY: Schedule = Schedule::Every(TimeDelta::minutes(15));

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
        .unwrap()
}

fn daily(hour: u32, minute: u32) -> Schedule {
    Schedule::Daily(NaiveTime::from_hms_opt(hour, minute, 0).unwrap())
}

/// 24 ต.ค. 2569 14:30 in Bangkok.
fn now() -> DateTime<Utc> {
    utc(2026, 10, 24, 7, 30, 0)
}

#[test]
fn a_periodic_job_is_next_due_a_period_after_it_started() {
    assert_eq!(EVERY.next(now(), Bangkok), utc(2026, 10, 24, 7, 45, 0));
    assert_eq!(EVERY.first(now(), Bangkok), now());
}

#[test]
fn a_daily_job_is_due_at_its_time_in_the_business_time_zone() {
    let at_three = daily(3, 0);
    // 02:59:59 in Bangkok is 19:59:59 UTC the day before
    let before = utc(2026, 10, 24, 19, 59, 59);
    assert_eq!(at_three.next(before, Bangkok), utc(2026, 10, 24, 20, 0, 0));
    assert_eq!(at_three.first(before, Bangkok), utc(2026, 10, 24, 20, 0, 0));
    // At its time exactly, the next run is a day later
    let on_time = utc(2026, 10, 24, 20, 0, 0);
    assert_eq!(at_three.next(on_time, Bangkok), utc(2026, 10, 25, 20, 0, 0));

    // Bangkok's day turns at 17:00 UTC, not at midnight UTC
    let late_evening = utc(2026, 10, 24, 16, 59, 0);
    assert_eq!(
        daily(0, 30).next(late_evening, Bangkok),
        utc(2026, 10, 24, 17, 30, 0)
    );
    assert_eq!(
        daily(0, 30).next(late_evening, chrono_tz::UTC),
        utc(2026, 10, 25, 0, 30, 0)
    );
}

#[test]
fn a_daily_time_the_clocks_skip_or_repeat_happens_once() {
    // New York skips 02:00–03:00 on 8 March 2026: 02:30 runs at 03:00 EDT
    let skipping = utc(2026, 3, 8, 5, 0, 0);
    assert_eq!(
        daily(2, 30).next(skipping, New_York),
        utc(2026, 3, 8, 7, 0, 0)
    );
    // ...and repeats 01:00–02:00 on 1 November 2026: 01:30 runs at the first, EDT
    let repeating = utc(2026, 11, 1, 4, 0, 0);
    assert_eq!(
        daily(1, 30).next(repeating, New_York),
        utc(2026, 11, 1, 5, 30, 0)
    );
    let after_first = utc(2026, 11, 1, 5, 30, 0);
    assert_eq!(
        daily(1, 30).next(after_first, New_York),
        utc(2026, 11, 2, 6, 30, 0)
    );
}

#[test]
fn a_schedule_is_recorded_with_what_it_depends_on() {
    assert_eq!(EVERY.describe(Bangkok), "every 900s");
    assert_eq!(daily(3, 0).describe(Bangkok), "daily 03:00 Asia/Bangkok");
    assert_ne!(daily(4, 0).describe(Bangkok), daily(3, 0).describe(Bangkok));
    assert_ne!(
        daily(3, 0).describe(New_York),
        daily(3, 0).describe(Bangkok)
    );
}

/// Work that counts its runs in `runs`.
fn count(runs: &Arc<AtomicUsize>) -> impl Future<Output = anyhow::Result<()>> + Send + use<> {
    let runs = Arc::clone(runs);
    async move {
        runs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn row(db: &DatabaseConnection) -> job::Model {
    job::Entity::find_by_id(NAME)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_due_job_runs_once_until_it_is_due_again() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let runs = Arc::new(AtomicUsize::new(0));
    let next = now() + TimeDelta::minutes(15);

    register(&db, NAME, EVERY, Bangkok, now()).await.unwrap();
    let ran = tick(&db, NAME, EVERY, Bangkok, now(), count(&runs)).await;
    assert_eq!(ran.unwrap(), Tick::Ran(next));
    let again = tick(&db, NAME, EVERY, Bangkok, now(), count(&runs)).await;
    assert_eq!(again.unwrap(), Tick::NotDue(next));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let stored = row(&db).await;
    assert_eq!(
        (stored.due_at, stored.last_run_at, stored.last_error),
        (next, Some(now()), None)
    );

    let due_again = tick(&db, NAME, EVERY, Bangkok, next, count(&runs)).await;
    assert_eq!(due_again.unwrap(), Tick::Ran(next + TimeDelta::minutes(15)));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn with_two_instances_each_run_happens_once() {
    let (container, pool) = testing::migrated().await;
    let (first, second) = (orm(pool), orm(testing::pool(&container).await));
    let runs = Arc::new(AtomicUsize::new(0));
    let next = now() + TimeDelta::minutes(15);
    register(&first, NAME, EVERY, Bangkok, now()).await.unwrap();
    register(&second, NAME, EVERY, Bangkok, now())
        .await
        .unwrap();

    // The second instance finds the run in progress, then done
    let (started, finish) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let work = {
        let (started, finish, runs) = (started.clone(), finish.clone(), count(&runs));
        async move {
            started.notify_one();
            finish.notified().await;
            runs.await
        }
    };
    let db = first.clone();
    let running = tokio::spawn(async move { tick(&db, NAME, EVERY, Bangkok, now(), work).await });
    started.notified().await;
    let busy = tick(&second, NAME, EVERY, Bangkok, now(), count(&runs)).await;
    assert_eq!(busy.unwrap(), Tick::Busy);
    finish.notify_one();
    assert_eq!(running.await.unwrap().unwrap(), Tick::Ran(next));
    let done = tick(&second, NAME, EVERY, Bangkok, now(), count(&runs)).await;
    assert_eq!(done.unwrap(), Tick::NotDue(next));
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // Both trying every run at the same moment
    let slow = |runs: &Arc<AtomicUsize>| {
        let runs = count(runs);
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            runs.await
        }
    };
    for run in 1..=5 {
        let at = now() + TimeDelta::minutes(15) * run;
        let (a, b) = tokio::join!(
            tick(&first, NAME, EVERY, Bangkok, at, slow(&runs)),
            tick(&second, NAME, EVERY, Bangkok, at, slow(&runs)),
        );
        let ran = [a.unwrap(), b.unwrap()]
            .into_iter()
            .filter(|tick| matches!(tick, Tick::Ran(_)))
            .count();
        assert_eq!(ran, 1, "run {run}");
    }
    assert_eq!(runs.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn a_run_cut_short_is_run_again_once_after_a_restart() {
    let (container, pool) = testing::migrated().await;
    let killed = testing::pool(&container).await;
    let (db, restarted) = (orm(killed.clone()), orm(pool));
    let runs = Arc::new(AtomicUsize::new(0));
    let next = now() + TimeDelta::minutes(15);
    register(&db, NAME, EVERY, Bangkok, now()).await.unwrap();

    // The first instance dies in the middle of the run
    let started = Arc::new(Notify::new());
    let work = {
        let started = started.clone();
        async move {
            started.notify_one();
            std::future::pending().await
        }
    };
    let running = tokio::spawn(async move { tick(&db, NAME, EVERY, Bangkok, now(), work).await });
    started.notified().await;
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    killed.close().await;

    // The next start runs it as soon as PostgreSQL has ended the dead instance's connection
    let deadline = Instant::now() + Duration::from_secs(5);
    let ran = loop {
        let tick = tick(&restarted, NAME, EVERY, Bangkok, now(), count(&runs)).await;
        match tick.unwrap() {
            Tick::Busy if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tick => break tick,
        }
    };
    assert_eq!(ran, Tick::Ran(next));
    let again = tick(&restarted, NAME, EVERY, Bangkok, now(), count(&runs)).await;
    assert_eq!(again.unwrap(), Tick::NotDue(next));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_run_is_tried_again_a_minute_later() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    register(&db, NAME, daily(3, 0), Bangkok, now())
        .await
        .unwrap();
    let due = row(&db).await.due_at;

    let failed = tick(&db, NAME, daily(3, 0), Bangkok, due, async {
        Err(anyhow!("ฐานข้อมูลไม่ตอบ"))
    })
    .await;
    let retry = due + RETRY_AFTER;
    assert_eq!(failed.unwrap(), Tick::Ran(retry));
    assert_eq!(row(&db).await.last_error.as_deref(), Some("ฐานข้อมูลไม่ตอบ"));

    // So is one that panicked
    let panicked = tick(&db, NAME, daily(3, 0), Bangkok, retry, async {
        panic!("the probe panicked on purpose")
    })
    .await;
    let retry_again = retry + RETRY_AFTER;
    assert_eq!(panicked.unwrap(), Tick::Ran(retry_again));
    let error = row(&db).await.last_error.unwrap();
    assert!(error.contains("panicked"), "{error}");

    // A run that succeeds clears the error, and the job is due at its time the next day
    let runs = Arc::new(AtomicUsize::new(0));
    let ran = tick(&db, NAME, daily(3, 0), Bangkok, retry_again, count(&runs)).await;
    assert_eq!(ran.unwrap(), Tick::Ran(due + TimeDelta::days(1)));
    let stored = row(&db).await;
    assert_eq!(
        (stored.last_run_at, stored.last_error),
        (Some(retry_again), None)
    );
}

#[tokio::test]
async fn a_job_keeps_its_due_time_across_restarts_until_its_schedule_changes() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    // 14:30 in Bangkok, so 03:00 is 20:00 UTC and 04:00 is 21:00 UTC
    let at_three = utc(2026, 10, 24, 20, 0, 0);
    let at_four = utc(2026, 10, 24, 21, 0, 0);

    register(&db, NAME, daily(3, 0), Bangkok, now())
        .await
        .unwrap();
    let stored = row(&db).await;
    assert_eq!(
        (stored.schedule.as_str(), stored.due_at),
        ("daily 03:00 Asia/Bangkok", at_three)
    );

    // Starting again later with the same schedule changes nothing
    let later = now() + TimeDelta::hours(1);
    register(&db, NAME, daily(3, 0), Bangkok, later)
        .await
        .unwrap();
    assert_eq!(row(&db).await, stored);

    // A new time, or a new time zone, reschedules the job
    register(&db, NAME, daily(4, 0), Bangkok, later)
        .await
        .unwrap();
    assert_eq!(row(&db).await.due_at, at_four);
    register(&db, NAME, daily(4, 0), chrono_tz::UTC, later)
        .await
        .unwrap();
    let stored = row(&db).await;
    assert_eq!(
        (stored.schedule.as_str(), stored.due_at),
        ("daily 04:00 UTC", utc(2026, 10, 25, 4, 0, 0))
    );
}
