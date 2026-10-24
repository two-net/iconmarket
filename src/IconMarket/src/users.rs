//! Users & onboarding (SRS §2.1): consent, registration, profile, roles and account
//! deletion. Built by #12, #19, #20 and #36.
//!
//! [`roles`] holds who is a buyer, seller and admin, and [`access`] the one authorization check
//! every action goes through.

#![allow(
    dead_code,
    reason = "follow, profile, menu and account handling are built by #17, #19 and #36"
)]

pub mod access;
pub mod roles;
#[cfg(test)]
mod tests;

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;

use crate::config::PrivacyConfig;
use crate::persistence::entity::user;

/// How long a fetched LINE profile is kept before it is fetched again (FR-LINE-10).
const PROFILE_REFRESH: TimeDelta = TimeDelta::days(1);

/// Whether to fetch a user's display name and picture from LINE now: on their first
/// interaction, then at most once a day (FR-LINE-10). A `follow` always fetches them.
pub fn profile_refresh_due(refreshed_at: Option<DateTimeUtc>, now: DateTimeUtc) -> bool {
    refreshed_at.is_none_or(|at| now - at >= PROFILE_REFRESH)
}

/// Whether `user` has finished onboarding, and so may use more than help and `#myid`
/// (FR-UI-12): they consented to the current privacy notice, so a new `privacy.noticeVersion`
/// asks everyone again (FR-ONB-02, NFR-PRIV-01). Onboarding (#19) adds the join code.
pub fn onboarded(user: &user::Model, privacy: &PrivacyConfig) -> bool {
    user.consent_version.as_deref() == Some(privacy.notice_version.as_str())
}
