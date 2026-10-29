//! "สถิติ" (FR-ADM-06): the market at a glance, for admins. Users, not counting deleted
//! accounts, and those new in the last 7 days; shops `APPROVED`, and those of them open now, as
//! [`takes_orders`](crate::catalog::shops::takes_orders) has it; orders placed today and in
//! the last 7 and 30 days by status, with the value of those now `COMPLETED`; the AI calls made
//! today against `ai.limits.globalDailyCalls`; and the pushes sent and given up this month
//! (FR-NOT-05).
//!
//! Days are the business time zone's (`app.timeZone`, DR-02): the last 7 days are today and
//! the 6 before it, each from midnight. The AI calls are the day's counter the limit is checked
//! against (FR-AI-09), so a call retried counts once. Admins read the same figures through the
//! assistant's `get_admin_overview`, which shows this card too (FR-AI-17).

#[cfg(test)]
mod tests;

use chrono::{Days, NaiveTime};
use chrono_tz::Tz;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, Iterable, PaginatorTrait, QueryFilter,
    QuerySelect,
};

use crate::ai;
use crate::config::Config;
use crate::dispatcher::Turn;
use crate::dispatcher::menu::Item;
use crate::line::Message;
use crate::notifications::templates::{Templates, labels};
use crate::notifications::{self, PushStats};
use crate::persistence::entity::{order, shop, user};
use crate::thai::{self, Money};
use crate::users::access::{self, authorize};

/// The periods orders are counted over, in days ending today: today, 7 days and 30 days
pub const PERIODS: [u64; 3] = [1, 7, 30];

/// What "สถิติ" shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statistics {
    /// Every account but deleted ones, following or not
    pub users: u64,
    /// Of those, the ones registered in the last 7 days
    pub new_users: u64,
    pub approved_shops: u64,
    /// Of those, the ones open now
    pub open_shops: u64,
    /// The orders placed in each of [`PERIODS`]
    pub orders: [Orders; 3],
    /// `None` while the AI is off
    pub ai_calls: Option<AiCalls>,
    /// This month's
    pub pushes: PushStats,
}

/// The orders placed in a period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orders {
    /// Its length in days, today included
    pub days: u64,
    /// How many have each status now, every status in the order of Table 4.6-A
    pub by_status: Vec<(order::Status, u64)>,
    /// The total of those now `COMPLETED`
    pub completed_value: Money,
}

impl Orders {
    pub fn total(&self) -> u64 {
        self.by_status.iter().map(|(_, count)| count).sum()
    }
}

/// The AI calls made today, and how many the day allows (FR-AI-09).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiCalls {
    pub today: u64,
    pub limit: u32,
}

/// A tap on "สถิติ", or on the card's button to see it again.
pub async fn tapped(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let statistics = gather(turn.db, config, turn.now).await?;
    Ok(vec![card(turn.templates, &statistics, turn.now)])
}

/// The card of `statistics`, as they were `at`, with a button to see them again.
pub fn card(templates: &Templates, statistics: &Statistics, at: DateTimeUtc) -> Message {
    let again = Item::Stats.postback().button(labels::REFRESH.clone());
    templates.statistics(statistics, at, again)
}

/// The statistics at `now`.
pub async fn gather(
    db: &impl ConnectionTrait,
    config: &Config,
    now: DateTimeUtc,
) -> Result<Statistics, DbErr> {
    let time_zone = config.app.time_zone;
    let users = || user::Entity::find().filter(user::Column::Status.ne(user::Status::Deleted));
    let approved = || shop::Entity::find().filter(shop::Column::Status.eq(shop::Status::Approved));
    let mut orders = Vec::with_capacity(PERIODS.len());
    for days in PERIODS {
        orders.push(placed(db, days, since(now, time_zone, days)).await?);
    }
    let ai_calls = match config.ai.primary {
        Some(_) => Some(AiCalls {
            today: ai::calls_today(db, now, time_zone).await?,
            limit: config.ai.limits.global_daily_calls,
        }),
        None => None,
    };
    Ok(Statistics {
        users: users().count(db).await?,
        new_users: users()
            .filter(user::Column::CreatedAt.gte(since(now, time_zone, 7)))
            .count(db)
            .await?,
        approved_shops: approved().count(db).await?,
        open_shops: approved()
            .filter(shop::Column::IsOpen.eq(true))
            .count(db)
            .await?,
        orders: orders.try_into().expect("one for each period"),
        ai_calls,
        pushes: notifications::stats(db, now, time_zone).await?,
    })
}

/// The orders placed from `from` on, a period of `days` days.
async fn placed(db: &impl ConnectionTrait, days: u64, from: DateTimeUtc) -> Result<Orders, DbErr> {
    let rows: Vec<(order::Status, i64, Option<Money>)> = order::Entity::find()
        .select_only()
        .column(order::Column::Status)
        .column_as(order::Column::Id.count(), "orders")
        .column_as(order::Column::Total.sum(), "value")
        .filter(order::Column::CreatedAt.gte(from))
        .group_by(order::Column::Status)
        .into_tuple()
        .all(db)
        .await?;
    let found = |status| rows.iter().find(|(found, ..)| *found == status);
    let by_status = order::Status::iter()
        .map(|status| {
            let count = found(status).map_or(0, |&(_, count, _)| count);
            (status, count.try_into().expect("a count is never negative"))
        })
        .collect();
    let completed_value = found(order::Status::Completed)
        .and_then(|&(.., value)| value)
        .unwrap_or(Money::ZERO);
    Ok(Orders {
        days,
        by_status,
        completed_value,
    })
}

/// The start of the `days` days ending on the day of `now`: midnight of the first of them, in
/// `time_zone`.
fn since(now: DateTimeUtc, time_zone: Tz, days: u64) -> DateTimeUtc {
    let today = now.with_timezone(&time_zone).date_naive();
    let first = today - Days::new(days - 1);
    thai::local_instant(first, NaiveTime::MIN, time_zone)
}
