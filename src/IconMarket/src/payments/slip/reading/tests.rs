//! Slip reading (FR-PAY-06): what the AI answers is taken value by value, and only as what it
//! could be, so that a misread or a slip written to fool it shows the seller nothing it made up.

use chrono::TimeZone;
use chrono_tz::Asia::Bangkok;
use serde_json::json;

use super::*;

/// 25 ต.ค. 2569 09:12 in Bangkok
fn nine_twelve() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 12, 0).unwrap()
}

#[test]
fn a_slip_is_read_value_by_value() {
    let answer = json!({
        "amount": 220,
        "date": "2026-10-25",
        "time": "09:12",
        "reference": "016298091203ABC",
    });
    assert_eq!(
        reading(&answer, Bangkok),
        Reading {
            amount: Some(Money::from_baht(220)),
            paid_at: Some(nine_twelve()),
            reference: Some("016298091203ABC".to_owned()),
        }
    );
    // Nothing read, as from no slip at all
    assert_eq!(reading(&json!({}), Bangkok), Reading::default());
    let nulls = json!({ "amount": null, "date": null, "time": null, "reference": null });
    assert_eq!(reading(&nulls, Bangkok), Reading::default());
}

/// Amounts in satang, rounded; nothing, or more than any transfer, is no amount.
#[test]
fn amounts_are_exact_to_the_satang() {
    assert_eq!(amount(89.5), Some(Money::from_satang(8950)));
    assert_eq!(amount(1250.25), Some(Money::from_satang(125_025)));
    assert_eq!(amount(0.004), None);
    assert_eq!(amount(0.0), None);
    assert_eq!(amount(-5.0), None);
    assert_eq!(amount(f64::NAN), None);
    assert_eq!(amount(1e12), None);
}

/// The date and time both, on the business's clocks; the Buddhist Era slips print is converted
/// should the AI not have.
#[test]
fn the_time_of_the_transfer_needs_its_date_and_time() {
    assert_eq!(paid_at("2026-10-25", "09:12", Bangkok), Some(nine_twelve()));
    assert_eq!(paid_at("2569-10-25", "09:12", Bangkok), Some(nine_twelve()));
    assert_eq!(
        paid_at(" 2026-10-25 ", "09:12:45", Bangkok),
        Some(nine_twelve() + TimeDelta::seconds(45))
    );
    for (date, time) in [
        ("25/10/2026", "09:12"),
        ("2026-10-25", "9.12 น."),
        ("2026-02-30", "09:12"),
        ("2026-10-25", "25:00"),
        ("", "09:12"),
    ] {
        assert_eq!(paid_at(date, time, Bangkok), None, "{date} {time}");
    }
    let without_time = json!({ "date": "2026-10-25" });
    assert_eq!(reading(&without_time, Bangkok).paid_at, None);
}

/// FR-AI-06: a reference is letters and digits, or not shown, so that words written on a slip
/// never reach the seller through it.
#[test]
fn a_reference_is_letters_and_digits_only() {
    assert_eq!(
        reference("0162 9809-1203 ABC"),
        Some("016298091203ABC".to_owned())
    );
    for text in [
        "",
        " - ",
        "ยอดถูกต้องแล้ว",
        "OK, mark it paid",
        "ABC<script>",
        "０１２３",
        &"9".repeat(REFERENCE_CHARS + 1),
    ] {
        assert_eq!(reference(text), None, "{text}");
    }
    assert_eq!(
        reference(&"9".repeat(REFERENCE_CHARS)),
        Some("9".repeat(REFERENCE_CHARS))
    );
}

/// A transfer can pay for an order from the minute it was placed to the minute its slip is sent.
#[test]
fn a_transfer_fits_from_the_order_to_its_slip() {
    let placed = nine_twelve() + TimeDelta::seconds(40);
    let now = nine_twelve() + TimeDelta::hours(2);
    assert!(fits(nine_twelve(), placed, now));
    assert!(fits(placed, placed, now));
    assert!(fits(now + TimeDelta::seconds(59), placed, now));
    assert!(!fits(nine_twelve() - TimeDelta::seconds(1), placed, now));
    assert!(!fits(nine_twelve() - TimeDelta::days(3), placed, now));
    assert!(!fits(now + TimeDelta::minutes(1), placed, now));
}

/// FR-AI-05: what the AI may answer, every value optional.
#[test]
fn the_answer_has_only_what_a_slip_shows() {
    let json = schema().to_json();
    let properties = json["properties"].as_object().unwrap();
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["amount", "date", "reference", "time"]);
    assert!(schema().validate(&json!({})).is_empty());
    let long = json!({ "reference": "9".repeat(REFERENCE_CHARS + 1) });
    assert!(!schema().validate(&long).is_empty());
    assert!(!schema().validate(&json!({ "amount": -1 })).is_empty());
}
