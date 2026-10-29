//! The orders placed today and in the last 7 and 30 days, by status, with the value of those now
//! `COMPLETED`: what "สถิติ" (FR-ADM-06) counts over the market, and "ยอดขาย" (FR-SHOP-11) over
//! one shop.
//!
//! Days are the business time zone's (`app.timeZone`, DR-02): the last 7 days are today and
//! the 6 before it, each from midnight. An order counts in the period it was placed in, by its
//! status now.

use chrono::{Days, NaiveTime};
use chrono_tz::Tz;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, Iterable, QueryFilter, QuerySelect,
};

use crate::persistence::entity::order;
use crate::thai::{self, Money};

/// The periods orders are counted over, in days ending today: today, 7 days and 30 days
pub const PERIODS: [u64; 3] = [1, 7, 30];

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

/// The orders placed from `from` on, a period of `days` days: those of shop `shop`, or of every
/// shop.
pub async fn placed(
    db: &impl ConnectionTrait,
    days: u64,
    from: DateTimeUtc,
    shop: Option<i64>,
) -> Result<Orders, DbErr> {
    let mut select = order::Entity::find()
        .select_only()
        .column(order::Column::Status)
        .column_as(order::Column::Id.count(), "orders")
        .column_as(order::Column::Total.sum(), "value")
        .filter(order::Column::CreatedAt.gte(from));
    if let Some(shop) = shop {
        select = select.filter(order::Column::ShopId.eq(shop));
    }
    let rows: Vec<(order::Status, i64, Option<Money>)> = select
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
pub fn since(now: DateTimeUtc, time_zone: Tz, days: u64) -> DateTimeUtc {
    let today = now.with_timezone(&time_zone).date_naive();
    let first = today - Days::new(days - 1);
    thai::local_instant(first, NaiveTime::MIN, time_zone)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use chrono_tz::Asia::Bangkok;

    use super::*;
    use crate::persistence::testing;

    fn utc(month: u32, day: u32, hour: u32) -> DateTimeUtc {
        Utc.with_ymd_and_hms(2026, month, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn periods_start_at_bangkok_midnight() {
        // 22 ต.ค. 2569 14:30 in Bangkok
        let now = testing::at();
        let today = utc(10, 21, 17);
        assert_eq!(since(now, Bangkok, 1), today);
        assert_eq!(since(now, Bangkok, 7), utc(10, 15, 17));
        assert_eq!(since(now, Bangkok, 30), utc(9, 22, 17));
        // Just after midnight in Bangkok, while it is still the day before in UTC
        assert_eq!(since(today, Bangkok, 1), today);
        let just_before = today - chrono::TimeDelta::seconds(1);
        assert_eq!(since(just_before, Bangkok, 1), utc(10, 20, 17));
    }
}
