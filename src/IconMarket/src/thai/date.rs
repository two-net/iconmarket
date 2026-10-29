//! Dates and times as Thai users read them (FR-UI-07, CON-11): `22 ต.ค. 2569 14:30`, with
//! Buddhist-Era years, in the business time zone (`app.timeZone`). Timestamps are kept in
//! UTC and converted only for display and for day boundaries (DR-02). Days are also read as
//! users type them: `พรุ่งนี้`, `15 พ.ย.`, `15/11/2569`.

use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveTime, TimeDelta, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use super::arabic_digits;

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

/// The months in full, as typed: `15 พฤศจิกายน`
const MONTH_NAMES: [&str; 12] = [
    "มกราคม",
    "กุมภาพันธ์",
    "มีนาคม",
    "เมษายน",
    "พฤษภาคม",
    "มิถุนายน",
    "กรกฎาคม",
    "สิงหาคม",
    "กันยายน",
    "ตุลาคม",
    "พฤศจิกายน",
    "ธันวาคม",
];

/// The days of the week, from Sunday
const WEEKDAYS: [&str; 7] = ["อา.", "จ.", "อ.", "พ.", "พฤ.", "ศ.", "ส."];

/// Buddhist Era (พ.ศ.) minus Common Era: 2026 is 2569.
pub const BUDDHIST_ERA_OFFSET: i32 = 543;

/// `22 ต.ค. 2569`
pub fn format_date(at: DateTime<Utc>, time_zone: Tz) -> String {
    date(&at.with_timezone(&time_zone))
}

/// `22 ต.ค. 2569`: a day, such as a pre-order's ready date, that is no instant in any zone
pub fn format_day(day: NaiveDate) -> String {
    date(&day)
}

/// `ส. 1 พ.ย.`: a day close enough for its weekday to say more than its year, as a button
/// offers it. [`parse_day`] reads it back.
pub fn format_short_day(day: NaiveDate) -> String {
    format!(
        "{} {} {}",
        WEEKDAYS[day.weekday().num_days_from_sunday() as usize],
        day.day(),
        MONTHS[day.month0() as usize]
    )
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

fn date(day: &impl Datelike) -> String {
    format!(
        "{} {} {}",
        day.day(),
        MONTHS[day.month0() as usize],
        day.year() + BUDDHIST_ERA_OFFSET
    )
}

/// The day `text` names, typed on `today`: `พรุ่งนี้` or `มะรืน(นี้)`; or a day and month, as
/// `15 พ.ย.`, `15 พย`, `15 พฤศจิกายน` or `15/11`, which may be preceded by `วันที่` or a
/// weekday (`ส. 1 พ.ย.`, as [`format_short_day`] writes it; the weekday is not checked) and
/// followed by a year: `2569` or `69` in the Buddhist Era, `2026` in the Common one. Without a
/// year, the next such day from `today` on. Digits may be Thai. Whether the day is one that may
/// be chosen is the caller's rule.
pub fn parse_day(text: &str, today: NaiveDate) -> Option<NaiveDate> {
    let text = arabic_digits(text.trim());
    match text.as_str() {
        "พรุ่งนี้" => return today.checked_add_days(Days::new(1)),
        "มะรืน" | "มะรืนนี้" => {
            return today.checked_add_days(Days::new(2));
        }
        _ => {}
    }
    let text = text.strip_prefix("วันที่").unwrap_or(&text).trim_start();
    let text = without_weekday(text);
    let (day, rest) = leading_number(text)?;
    let rest = rest.trim_start();
    let (month, year) = if let Some(rest) = rest.strip_prefix(['/', '-']) {
        let (month, rest) = leading_number(rest)?;
        let year = match rest.strip_prefix(['/', '-']) {
            Some(year) => Some(year),
            None if rest.is_empty() => None,
            None => return None,
        };
        (month, year)
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (name, year) = rest.split_at(end);
        let year = year.trim_start();
        (month_named(name)?, (!year.is_empty()).then_some(year))
    };
    match year {
        Some(year) => NaiveDate::from_ymd_opt(common_era(year)?, month, day),
        None => [today.year(), today.year() + 1]
            .into_iter()
            .filter_map(|year| NaiveDate::from_ymd_opt(year, month, day))
            .find(|&named| named >= today),
    }
}

/// `text` without a weekday before the day: `ส. 1 พ.ย.` is `1 พ.ย.`.
fn without_weekday(text: &str) -> &str {
    let Some((first, rest)) = text.split_once(char::is_whitespace) else {
        return text;
    };
    let first = first.trim_end_matches('.');
    if WEEKDAYS
        .iter()
        .any(|weekday| weekday.trim_end_matches('.') == first)
    {
        rest.trim_start()
    } else {
        text
    }
}

/// The number `text` starts with, of at most 4 ASCII digits, and what follows it.
fn leading_number(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if !(1..=4).contains(&end) {
        return None;
    }
    let (number, rest) = text.split_at(end);
    Some((number.parse().ok()?, rest))
}

/// The month `name` names, 1 – 12: its abbreviation with or without dots, or in full.
fn month_named(name: &str) -> Option<u32> {
    let name = name.replace('.', "");
    let month = MONTHS
        .iter()
        .zip(MONTH_NAMES)
        .position(|(short, full)| short.replace('.', "") == name || full == name)?;
    u32::try_from(month + 1).ok()
}

/// The Common-Era year `text` names: `2569` or `69` in the Buddhist Era, or `2026`.
fn common_era(text: &str) -> Option<i32> {
    let (year, rest) = leading_number(text.trim())?;
    if !rest.trim().is_empty() {
        return None;
    }
    let year = i32::try_from(year).ok()?;
    match text.trim().len() {
        2 => Some(2500 + year - BUDDHIST_ERA_OFFSET),
        4 if year >= 2400 => Some(year - BUDDHIST_ERA_OFFSET),
        4 => Some(year),
        _ => None,
    }
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

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn days_are_written_as_dates_are() {
        assert_eq!(format_day(day(2026, 11, 5)), "5 พ.ย. 2569");
        assert_eq!(format_day(day(2027, 1, 1)), "1 ม.ค. 2570");
        assert_eq!(format_short_day(day(2026, 10, 31)), "ส. 31 ต.ค.");
        assert_eq!(format_short_day(day(2026, 11, 1)), "อา. 1 พ.ย.");
        assert_eq!(format_short_day(day(2026, 11, 5)), "พฤ. 5 พ.ย.");
    }

    /// FR-PROD-11: a ready date as a seller types it, or taps it
    #[test]
    fn days_are_read_as_typed() {
        let today = day(2026, 10, 25);
        let read = |text: &str| parse_day(text, today);
        assert_eq!(read("พรุ่งนี้"), Some(day(2026, 10, 26)));
        assert_eq!(read(" มะรืน "), Some(day(2026, 10, 27)));
        assert_eq!(read("มะรืนนี้"), Some(day(2026, 10, 27)));
        for typed in [
            "15 พ.ย.",
            "15 พย",
            "15พ.ย.",
            "15 พฤศจิกายน",
            "๑๕ พ.ย.",
            "15/11",
            "15-11",
            "วันที่ 15 พ.ย.",
            "อา. 15 พ.ย.",
            "15 พ.ย. 2569",
            "15 พ.ย. 69",
            "15 พ.ย. 2026",
            "15/11/2569",
            "15/11/69",
            "๑๕/๑๑/๒๕๖๙",
        ] {
            assert_eq!(read(typed), Some(day(2026, 11, 15)), "{typed}");
        }
        // Every button offered reads back as its day
        for ahead in 1..=10 {
            let offered = today + Days::new(ahead);
            assert_eq!(read(&format_short_day(offered)), Some(offered));
        }
    }

    #[test]
    fn a_day_without_a_year_is_the_next_one() {
        let today = day(2026, 12, 30);
        assert_eq!(parse_day("30 ธ.ค.", today), Some(today));
        assert_eq!(parse_day("2 ม.ค.", today), Some(day(2027, 1, 2)));
        assert_eq!(parse_day("ศ. 1 ม.ค.", today), Some(day(2027, 1, 1)));
        assert_eq!(parse_day("29 ธ.ค.", today), Some(day(2027, 12, 29)));
        // A day the next year has, but not this one
        assert_eq!(parse_day("29/2", day(2027, 3, 1)), Some(day(2028, 2, 29)));
        // With a year, as it says, even if past
        assert_eq!(parse_day("29 ธ.ค. 2569", today), Some(day(2026, 12, 29)));
    }

    #[test]
    fn what_is_no_day_is_refused() {
        let today = day(2026, 10, 25);
        for typed in [
            "",
            "วันศุกร์",
            "พ.ย.",
            "15",
            "15 พฤศ",
            "32 ต.ค.",
            "31/11",
            "15/13",
            "15/11/",
            "15/11/2569/1",
            "15 พ.ย. 569",
            "15 พ.ย. ปีหน้า",
            "15 พ.ย. 2569 เช้า",
            "12345 พ.ย.",
            "-1 พ.ย.",
            "1.5 พ.ย.",
        ] {
            assert_eq!(parse_day(typed, today), None, "{typed}");
        }
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
