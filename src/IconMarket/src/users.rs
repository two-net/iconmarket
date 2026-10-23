//! Users & onboarding (SRS §2.1): consent, registration, profile, roles and account
//! deletion. Built by #12, #19, #20 and #36.

#![allow(
    dead_code,
    reason = "follow and interaction handling are built by #14 and #19"
)]

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;

/// How long a fetched LINE profile is kept before it is fetched again (FR-LINE-10).
const PROFILE_REFRESH: TimeDelta = TimeDelta::days(1);

/// Whether to fetch a user's display name and picture from LINE now: on their first
/// interaction, then at most once a day (FR-LINE-10). A `follow` always fetches them.
pub fn profile_refresh_due(refreshed_at: Option<DateTimeUtc>, now: DateTimeUtc) -> bool {
    refreshed_at.is_none_or(|at| now - at >= PROFILE_REFRESH)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn profiles_are_refreshed_at_most_once_a_day() {
        let now = Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap();
        assert!(profile_refresh_due(None, now));
        assert!(!profile_refresh_due(Some(now), now));
        assert!(!profile_refresh_due(Some(now - TimeDelta::hours(23)), now));
        assert!(profile_refresh_due(Some(now - TimeDelta::hours(24)), now));
        assert!(profile_refresh_due(Some(now - TimeDelta::days(30)), now));
    }
}
