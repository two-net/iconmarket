//! Dates and times as Thai users read them (FR-UI-07, CON-11): `22 ต.ค. 2569 14:30`, with
//! Buddhist-Era years, in the business time zone (`app.timeZone`). Timestamps are kept in
//! UTC and converted only for display and for day boundaries (DR-02).

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, TimeDelta, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

const MONTHS: [&str; 12] = [
    "ม.ค.",
    "ก.พ.",
    "มี.ค.",
    "เม.ย.",
    "พ.ค.",
    "มิ.ย.",
    "ก.ค.",
    "ส.ค.",
    "ก.ย.",
    "ต.ค.",
    "พ.ย.",
    "ธ.ค.",
];

/// Buddhist Era (พ.ศ.) minus Common Era: 2026 is 2569.
const BUDDHIST_ERA_OFFSET: i32 = 543;

/// `22 ต.ค. 2569`
pub fn format_date(at: DateTime<Utc>, time_zone: Tz) -> String {
    date(&at.with_timezone(&time_zone))
}

/// `22 ต.ค. 2569 14:30`
pub fn format_date_time(at: DateTime<Utc>, time_zone: Tz) -> String {
    let local = at.with_timezone(&time_zone);
    format!("{} {:02}:{:02}", date(&local), local.hour(), local.minute())
}

/// `20261022`: the day it is at `at` in `time_zone`, as daily counters and order numbers name
/// it (§5.1, FR-ORD-01)
pub fn compact_date(at: DateTime<Utc>, time_zone: Tz) -> String {
    let day = at.with_timezone(&time_zone);
    format!("{:04}{:02}{:02}", day.year(), day.month(), day.day())
}

/// The first instant at which the clocks in `time_zone` show `time` on `date`: the earlier one
/// when the clocks go back, and the end of the gap when they skip `time`.
pub fn local_instant(date: NaiveDate, time: NaiveTime, time_zone: Tz) -> DateTime<Utc> {
    let mut at = date.and_time(time);
    loop {
        if let Some(instant) = time_zone.from_local_datetime(&at).earliest() {
            return instant.to_utc();
        }
        // Clocks change on the minute
        at += TimeDelta::minutes(1);
    }
}

fn date(local: &DateTime<Tz>) -> String {
    format!(
        "{} {} {}",
        local.day(),
        MONTHS[local.month0() as usize],
        local.year() + BUDDHIST_ERA_OFFSET
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Asia::Bangkok;

    use super::*;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn bangkok_time_and_buddhist_era() {
        let at = utc(2026, 10, 22, 7, 30);
        assert_eq!(format_date_time(at, Bangkok), "22 ต.ค. 2569 14:30");
        assert_eq!(format_date(at, Bangkok), "22 ต.ค. 2569");
    }

    #[test]
    fn the_day_turns_at_bangkok_midnight() {
        let at = utc(2026, 12, 31, 17, 0);
        assert_eq!(format_date_time(at, Bangkok), "1 ม.ค. 2570 00:00");
        assert_eq!(format_date(at, chrono_tz::UTC), "31 ธ.ค. 2569");
    }

    /// §5.1, FR-ORD-01: a day is named in the Gregorian calendar, turning at Bangkok midnight.
    #[test]
    fn compact_dates_name_the_local_day() {
        assert_eq!(compact_date(utc(2026, 10, 22, 7, 30), Bangkok), "20261022");
        assert_eq!(compact_date(utc(2026, 12, 31, 16, 59), Bangkok), "20261231");
        assert_eq!(compact_date(utc(2026, 12, 31, 17, 0), Bangkok), "20270101");
        assert_eq!(compact_date(utc(2027, 3, 5, 2, 0), Bangkok), "20270305");
    }

    #[test]
    fn day_without_and_time_with_leading_zeros() {
        assert_eq!(
            format_date_time(utc(2026, 3, 5, 2, 5), Bangkok),
            "5 มี.ค. 2569 09:05"
        );
    }

    #[test]
    fn every_month() {
        let months: Vec<_> = (1..=12)
            .map(|month| format_date(utc(2026, month, 15, 5, 0), Bangkok))
            .collect();
        assert_eq!(
            months,
            [
                "15 ม.ค. 2569",
                "15 ก.พ. 2569",
                "15 มี.ค. 2569",
                "15 เม.ย. 2569",
                "15 พ.ค. 2569",
                "15 มิ.ย. 2569",
                "15 ก.ค. 2569",
                "15 ส.ค. 2569",
                "15 ก.ย. 2569",
                "15 ต.ค. 2569",
                "15 พ.ย. 2569",
                "15 ธ.ค. 2569",
            ]
        );
    }
}
