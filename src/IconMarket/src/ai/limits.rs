//! The daily AI limits (FR-AI-09): `ai.limits.perUserDailyCalls` and
//! `ai.limits.globalDailyCalls`, kept in daily counters (§5.1) so they reset at midnight in the
//! business time zone (`app.timeZone`). A request counts once, however many tries it takes.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, TransactionTrait};

use crate::persistence::entity::counter;
use crate::thai;

/// The day's counter keys for a call at `now`: everyone's, and the user's when it is made for
/// one. In the style of `order:20261022`.
pub(super) fn keys(
    now: DateTime<Utc>,
    time_zone: Tz,
    user_id: Option<i64>,
) -> (String, Option<String>) {
    let global = format!("ai:{}", thai::compact_date(now, time_zone));
    let user = user_id.map(|id| format!("{global}:u{id}"));
    (global, user)
}

/// Counts one call against both limits, or against neither when either is reached: then
/// `false`, and the call must not be made.
pub(super) async fn reserve(
    db: &DatabaseConnection,
    (global, user): (String, Option<String>),
    global_limit: u32,
    user_limit: u32,
) -> Result<bool, DbErr> {
    let txn = db.begin().await?;
    let counters = [
        Some((global, global_limit)),
        user.map(|key| (key, user_limit)),
    ];
    for (key, limit) in counters.into_iter().flatten() {
        if !increment(&txn, key, limit).await? {
            txn.rollback().await?;
            return Ok(false);
        }
    }
    txn.commit().await?;
    Ok(true)
}

/// Adds 1 to the counter at `key` unless it has reached `limit`, in one statement so
/// concurrent calls can't both take the last one. `false` when it had.
async fn increment(db: &impl ConnectionTrait, key: String, limit: u32) -> Result<bool, DbErr> {
    let value = Expr::col((counter::Entity, counter::Column::Value));
    let first = counter::ActiveModel {
        key: Set(key),
        value: Set(1),
    };
    let added = counter::Entity::insert(first)
        .on_conflict(
            OnConflict::column(counter::Column::Key)
                .value(counter::Column::Value, value.clone().add(1))
                .action_and_where(value.lt(i64::from(limit)))
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    Ok(added == 1)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Asia::Bangkok;

    use super::*;

    #[test]
    fn the_day_turns_at_bangkok_midnight() {
        let before = Utc.with_ymd_and_hms(2026, 10, 24, 16, 59, 59).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 10, 24, 17, 0, 0).unwrap();
        assert_eq!(
            keys(before, Bangkok, Some(42)),
            ("ai:20261024".to_owned(), Some("ai:20261024:u42".to_owned()))
        );
        assert_eq!(
            keys(after, Bangkok, Some(42)),
            ("ai:20261025".to_owned(), Some("ai:20261025:u42".to_owned()))
        );
        assert_eq!(
            keys(after, chrono_tz::UTC, None),
            ("ai:20261024".to_owned(), None)
        );
    }
}
