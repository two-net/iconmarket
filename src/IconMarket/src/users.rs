//! Users & onboarding (SRS §2.1): consent, registration, profile, roles and account
//! deletion. Built by #12, #19, #20 and #36.
//!
//! [`roles`] holds who is a buyer, seller and admin, and [`access`] the one authorization check
//! every action goes through.

#![allow(
    dead_code,
    reason = "follow, postback and menu handling are built by #13, #14, #17 and #19"
)]

pub mod access;
pub mod roles;
#[cfg(test)]
mod tests;

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;

/// How long a fetched LINE profile is kept before it is fetched again (FR-LINE-10).
const PROFILE_REFRESH: TimeDelta = TimeDelta::days(1);

/// Whether to fetch a user's display name and picture from LINE now: on their first
/// interaction, then at most once a day (FR-LINE-10). A `follow` always fetches them.
pub fn profile_refresh_due(refreshed_at: Option<DateTimeUtc>, now: DateTimeUtc) -> bool {
    refreshed_at.is_none_or(|at| now - at >= PROFILE_REFRESH)
}
