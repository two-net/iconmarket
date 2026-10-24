//! Users & onboarding (SRS §2.1): consent, registration, profile, roles and account
//! deletion. Built by #12, #19, #20 and #36.
//!
//! [`roles`] holds who is a buyer, seller and admin, [`access`] the one authorization check
//! every action goes through, [`onboarding`] what a user does before anything else,
//! [`address`] the address asked for when first needed, and [`profile`] "ข้อมูลของฉัน".

#![allow(
    dead_code,
    reason = "account deletion is built by #36; shop applications (#23) and checkout (#32) first ask for the address"
)]

pub mod access;
pub mod address;
pub mod onboarding;
pub mod profile;
pub mod roles;
#[cfg(test)]
mod tests;

use chrono::TimeDelta;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

use crate::config::{PrivacyConfig, VillageConfig};
use crate::line::Profile;
use crate::persistence::entity::user;

/// How long a fetched LINE profile is kept before it is fetched again (FR-LINE-10).
const PROFILE_REFRESH: TimeDelta = TimeDelta::days(1);

/// Whether to fetch a user's display name and picture from LINE now: on their first
/// interaction, then at most once a day (FR-LINE-10). A `follow` always fetches them.
pub fn profile_refresh_due(refreshed_at: Option<DateTimeUtc>, now: DateTimeUtc) -> bool {
    refreshed_at.is_none_or(|at| now - at >= PROFILE_REFRESH)
}

/// Stores the display name and picture fetched from LINE at `now` for user `user_id`. `None`,
/// when LINE wouldn't say, keeps what was stored, but counts as fetched, so it is asked for
/// again only tomorrow (FR-LINE-10).
pub async fn store_profile(
    db: &impl ConnectionTrait,
    user_id: i64,
    profile: Option<Profile>,
    now: DateTimeUtc,
) -> Result<(), DbErr> {
    let mut row = user::ActiveModel {
        profile_refreshed_at: Set(Some(now)),
        ..Default::default()
    };
    if let Some(profile) = profile {
        row.display_name = Set(Some(profile.display_name));
        row.picture_url = Set(profile.picture_url);
    }
    user::Entity::update_many()
        .set(row)
        .filter(user::Column::Id.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

/// What a user has still to do before they may use more than help and `#myid` (FR-UI-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// Consent to the current privacy notice (FR-ONB-02)
    Consent,
    /// Enter `village.joinCode` (FR-ONB-03)
    JoinCode,
}

/// What `user` has still to do, in order: `None` once they are onboarded. Consent is to the
/// current privacy notice, so a new `privacy.noticeVersion` asks everyone again (NFR-PRIV-01).
/// The join code is asked for once, after consent, from everyone who hasn't entered it while
/// one is set.
pub fn pending(
    user: &user::Model,
    privacy: &PrivacyConfig,
    village: &VillageConfig,
) -> Option<Pending> {
    if user.consent_version.as_deref() != Some(privacy.notice_version.as_str()) {
        Some(Pending::Consent)
    } else if village.join_code.is_some() && user.joined_at.is_none() {
        Some(Pending::JoinCode)
    } else {
        None
    }
}
