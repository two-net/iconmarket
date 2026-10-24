//! The per-user rate limit (NFR-SEC-05): a user who sends more than
//! `limits.messagesPerUserPerMinute` messages and taps in a minute is told once to slow down,
//! and their further events are acknowledged and otherwise ignored until they do.
//!
//! The limit is judged from the inbox alone, by when the user's events were received, so an
//! event is judged the same however often it is handled, by whichever instance, and a backlog
//! handled after a restart is judged as it arrived.

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};

use crate::persistence::entity::webhook_event;

/// The span the limit counts events in
const WINDOW: TimeDelta = TimeDelta::minutes(1);

/// The event types that count: what users send and tap
const COUNTED: [&str; 2] = ["message", "postback"];

/// What becomes of an event under the limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Within the limit: handled as usual
    Allowed,
    /// The first over it: answered with the slow-down notice only
    Notice,
    /// Over it after the notice: acknowledged and otherwise ignored
    Ignored,
}

/// Whether `event`, a message or postback, is within its user's limit of `per_minute`.
pub async fn check(
    db: &impl ConnectionTrait,
    event: &webhook_event::Model,
    per_minute: u32,
) -> Result<Verdict, DbErr> {
    let received: Vec<DateTimeUtc> = webhook_event::Entity::find()
        .select_only()
        .column(webhook_event::Column::ReceivedAt)
        .filter(webhook_event::Column::UserKey.eq(&event.user_key))
        .filter(webhook_event::Column::Id.lte(event.id))
        .filter(webhook_event::Column::EventType.is_in(COUNTED))
        .order_by_desc(webhook_event::Column::Id)
        .limit(u64::from(per_minute) + 2)
        .into_tuple()
        .all(db)
        .await?;
    Ok(judge(&received, per_minute))
}

/// The verdict on the first of `received`: when an event and the user's events counted before
/// it were received, newest first. An event is over the limit when more than `per_minute`
/// events, itself included, arrived within the minute up to it; only the first event over it
/// gets the notice, so each stretch over the limit is answered once.
fn judge(received: &[DateTimeUtc], per_minute: u32) -> Verdict {
    let limit = usize::try_from(per_minute).unwrap_or(usize::MAX);
    let over = |i: usize| {
        i.checked_add(limit)
            .and_then(|oldest| received.get(oldest))
            .is_some_and(|&oldest| received[i] - oldest < WINDOW)
    };
    match (over(0), over(1)) {
        (false, _) => Verdict::Allowed,
        (true, false) => Verdict::Notice,
        (true, true) => Verdict::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn at(seconds: i64) -> DateTimeUtc {
        Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap() + TimeDelta::seconds(seconds)
    }

    /// The verdict on each of events received at `seconds`, oldest first, as each would be
    /// judged when handled.
    fn verdicts(seconds: &[i64], per_minute: u32) -> Vec<Verdict> {
        let received: Vec<_> = seconds.iter().rev().map(|&s| at(s)).collect();
        (0..received.len())
            .rev()
            .map(|i| judge(&received[i..], per_minute))
            .collect()
    }

    use Verdict::{Allowed, Ignored, Notice};

    #[test]
    fn events_within_the_limit_are_allowed() {
        assert_eq!(verdicts(&[0, 1, 2], 3), [Allowed; 3]);
        assert_eq!(verdicts(&[0], 1), [Allowed]);
        assert_eq!(verdicts(&[], 1), Vec::<Verdict>::new());
    }

    /// AC: a user over the limit gets exactly one slow-down notice.
    #[test]
    fn the_first_event_over_the_limit_gets_the_notice_and_the_rest_are_ignored() {
        assert_eq!(
            verdicts(&[0, 0, 0, 0, 0, 0], 3),
            [Allowed, Allowed, Allowed, Notice, Ignored, Ignored]
        );
        assert_eq!(
            verdicts(&[0, 10, 20, 30], 3),
            [Allowed, Allowed, Allowed, Notice]
        );
    }

    #[test]
    fn steady_sending_over_the_limit_gets_no_second_notice() {
        // One every 15 seconds for three minutes against a limit of 3 a minute: 4 within the
        // minute up to each event once going
        let seconds: Vec<_> = (0..12).map(|i| i * 15).collect();
        let verdicts = verdicts(&seconds, 3);
        assert_eq!(verdicts[..4], [Allowed, Allowed, Allowed, Notice]);
        assert!(verdicts[4..].iter().all(|&v| v == Ignored), "{verdicts:?}");
    }

    #[test]
    fn a_quiet_minute_resets_the_limit() {
        // The fourth event after the pause has four within its minute: a new stretch over it
        assert_eq!(
            verdicts(&[0, 1, 2, 3, 63, 64, 65, 66, 67], 3),
            [
                Allowed, Allowed, Allowed, Notice, Allowed, Allowed, Allowed, Notice, Ignored
            ]
        );
        // Over again after a pause long enough to drop below it: a new stretch, a new notice
        assert_eq!(
            verdicts(&[0, 0, 0, 0, 120, 120, 120, 120], 3),
            [
                Allowed, Allowed, Allowed, Notice, Allowed, Allowed, Allowed, Notice
            ]
        );
    }

    #[test]
    fn a_minute_apart_is_outside_the_window() {
        assert_eq!(verdicts(&[0, 60], 1), [Allowed, Allowed]);
        assert_eq!(verdicts(&[0, 59], 1), [Allowed, Notice]);
    }
}
