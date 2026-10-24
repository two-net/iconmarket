//! Onboarding (§4.1): what a user does before anything but help and `#myid` works (FR-UI-12).
//!
//! Following the OA registers the user, or makes them a follower again, and greets them with
//! the village's name (FR-ONB-01). They then accept the privacy notice by tapping "ยอมรับ",
//! which records the version they accepted and when (FR-ONB-02); a new `privacy.noticeVersion`
//! asks everyone again at their next message (NFR-PRIV-01). If `village.joinCode` is set, they
//! then type it; 5 wrong codes within an hour block further attempts for an hour (FR-ONB-03).
//!
//! What a user has still to do follows from their row ([`pending`](super::pending)), so
//! onboarding is no guided flow: it has no draft to keep and never expires. Until it is done,
//! the dispatcher hands it everything but help and `#myid`.
//!
//! The button that accepts the notice carries a fingerprint of the version shown, so a tap on
//! a notice since replaced shows the current one rather than accepting what the user hasn't
//! read (FR-UI-13).

#[cfg(test)]
mod tests;

use chrono::TimeDelta;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use super::Pending;
use super::access::Denied;
use crate::config::{Config, PaymentMethod, PrivacyConfig};
use crate::dispatcher::Turn;
use crate::dispatcher::menu;
use crate::dispatcher::postback::Postback;
use crate::line::Message;
use crate::notifications::templates::{PrivacyFacts, PrivacyNotice, Templates, labels};
use crate::persistence::entity::{join_attempt, user};
use crate::thai;

/// The action of the button that accepts the privacy notice (FR-ONB-02). Its `v` is the
/// [`fingerprint`] of the version shown.
pub const ACCEPT: &str = "onboarding.accept";

/// Wrong join codes that block further attempts (FR-ONB-03)
const MAX_WRONG: usize = 5;
/// The span wrong join codes are counted in
const WINDOW: TimeDelta = TimeDelta::hours(1);
/// How long attempts are blocked, from the last wrong code
const BLOCK: TimeDelta = TimeDelta::hours(1);

/// What a user not yet onboarded sent, as onboarding takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Said<'a> {
    /// Text that is no global keyword: a join code, perhaps
    Text(&'a str),
    /// Anything else, such as a keyword, a photo or a sticker: never a join code
    Other,
}

/// The answer to `follow` (FR-ONB-01), once the dispatcher has registered the user and fetched
/// their profile: they follow again if they had unfollowed, and are greeted, then asked for what
/// they have still to do, or shown the main menu.
pub async fn followed(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    let user = &turn.actor.user;
    if !user.is_following {
        user::Entity::update_many()
            .col_expr(user::Column::IsFollowing, Expr::value(true))
            .col_expr(user::Column::UpdatedAt, Expr::value(turn.now))
            .filter(user::Column::Id.eq(user.id))
            .exec(turn.db)
            .await?;
        info!("user follows again");
    }
    let (name, village) = (user.display_name.as_deref(), config.village.name.as_str());
    Ok(
        match super::pending(user, &config.privacy, &config.village) {
            None => vec![
                turn.templates.welcome_back(name, village),
                menu::main_menu(&turn.actor),
            ],
            Some(pending) => vec![
                turn.templates.welcome(name, village),
                ask(turn, config, pending),
            ],
        },
    )
}

/// The answer to what a user not yet onboarded sent (FR-UI-12 step 2): the privacy notice until
/// they accept it, then the join code, checked if they typed one.
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    said: Said<'_>,
) -> Result<Vec<Message>, DbErr> {
    let pending = super::pending(&turn.actor.user, &config.privacy, &config.village);
    match (pending, said, &config.village.join_code) {
        (Some(Pending::JoinCode), Said::Text(typed), Some(code)) => join(turn, typed, code).await,
        (Some(pending), ..) => Ok(vec![ask(turn, config, pending)]),
        (None, ..) => Ok(vec![menu::main_menu(&turn.actor)]),
    }
}

/// A tap on one of onboarding's buttons. "ยอมรับ" accepts the notice it was shown on, if that
/// is still the current one; a tap on a notice since replaced shows the current one instead
/// (FR-UI-13, NFR-PRIV-01), and a repeated tap changes nothing.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if postback.action() != ACCEPT {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    }
    let privacy = &config.privacy;
    let current = postback.int("v") == Some(fingerprint(&privacy.notice_version));
    match super::pending(&turn.actor.user, privacy, &config.village) {
        Some(Pending::Consent) if current => consent(turn, config).await,
        Some(Pending::Consent) => {
            debug!("a notice since replaced was accepted; the current one shown");
            Ok(vec![ask(turn, config, Pending::Consent)])
        }
        Some(pending) => Ok(vec![ask(turn, config, pending)]),
        None => Ok(vec![menu::main_menu(&turn.actor)]),
    }
}

/// The answer to `#myid` (FR-ONB-08): the sender's own LINE user ID, for `admin.lineUserIds`.
pub fn my_id(turn: &Turn<'_>) -> Vec<Message> {
    match &turn.actor.user.line_user_id {
        Some(line_user_id) => turn.templates.my_id(line_user_id),
        // Only a deleted account has none, and it sends no events
        None => vec![turn.templates.denied(Denied::NotFound)],
    }
}

/// What asks the user for `pending`.
fn ask(turn: &Turn<'_>, config: &Config, pending: Pending) -> Message {
    match pending {
        Pending::Consent => {
            let updated = turn.actor.user.consent_version.is_some();
            notice(turn.templates, config, updated)
        }
        Pending::JoinCode => {
            let village = &config.village;
            turn.templates
                .ask_join_code(&village.name, village.admin_contact.as_deref())
        }
    }
}

/// The postback of the button that accepts the notice of `privacy.noticeVersion` (FR-ONB-02).
pub fn accept(privacy: &PrivacyConfig) -> Postback {
    Postback::new(ACCEPT).with_int("v", fingerprint(&privacy.notice_version))
}

/// The current privacy notice (FR-ONB-02, NFR-PRIV-02), naming the AI providers that what users
/// send the AI goes to, with a button that accepts this version. `updated` when the user
/// accepted an earlier one, and so is asked again (NFR-PRIV-01).
pub fn notice(templates: &Templates, config: &Config, updated: bool) -> Message {
    templates.privacy_notice(&PrivacyNotice {
        facts: privacy_facts(config),
        updated,
        accept: accept(&config.privacy).button(labels::ACCEPT.clone()),
    })
}

/// What the privacy notice says (NFR-PRIV-02), as configured: help's "ความเป็นส่วนตัว" says
/// the same (FR-HELP-01).
pub fn privacy_facts(config: &Config) -> PrivacyFacts<'_> {
    let privacy = &config.privacy;
    let mut ai_providers = Vec::new();
    for endpoint in config.ai.primary.iter().chain(&config.ai.fallback) {
        if !ai_providers.contains(&endpoint.provider) {
            ai_providers.push(endpoint.provider);
        }
    }
    let slips = config.payment.methods.contains(&PaymentMethod::PromptPay);
    PrivacyFacts {
        text: &privacy.notice_text,
        version: &privacy.notice_version,
        url: privacy.notice_url.as_ref().map(|url| url.as_str()),
        ai_providers,
        ai_message_days: config.retention.ai_message_days,
        slip_days: slips.then_some(config.retention.slip_days),
    }
}

/// Records that the user accepted the current notice now (FR-ONB-02), then asks for the join
/// code, or shows the main menu.
async fn consent(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    let version = &config.privacy.notice_version;
    let user = user::Model {
        consent_version: Some(version.clone()),
        consented_at: Some(turn.now),
        ..turn.actor.user.clone()
    };
    user::Entity::update_many()
        .set(user::ActiveModel {
            consent_version: Set(user.consent_version.clone()),
            consented_at: Set(user.consented_at),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(user::Column::Id.eq(user.id))
        .exec(turn.db)
        .await?;
    info!(notice_version = %version, "privacy notice accepted");
    Ok(
        match super::pending(&user, &config.privacy, &config.village) {
            Some(pending) => vec![ask(turn, config, pending)],
            None => vec![turn.templates.ready(), menu::main_menu(&turn.actor)],
        },
    )
}

/// Checks `typed` against the join `code` (FR-ONB-03): unless attempts are blocked, the right
/// code completes onboarding, and a wrong one is counted.
async fn join(turn: &Turn<'_>, typed: &str, code: &str) -> Result<Vec<Message>, DbErr> {
    let (db, user_id, now) = (turn.db, turn.actor.user.id, turn.now);
    if let Some(until) = blocked_until(&wrong_codes(db, user_id).await?, now) {
        debug!("join code typed while blocked; not checked");
        return Ok(vec![turn.templates.join_code_blocked(until)]);
    }
    if is_join_code(typed, code) {
        user::Entity::update_many()
            .col_expr(user::Column::JoinedAt, Expr::value(now))
            .col_expr(user::Column::UpdatedAt, Expr::value(now))
            .filter(user::Column::Id.eq(user_id))
            .exec(db)
            .await?;
        join_attempt::Entity::delete_many()
            .filter(join_attempt::Column::UserId.eq(user_id))
            .exec(db)
            .await?;
        info!("join code accepted");
        return Ok(vec![turn.templates.ready(), menu::main_menu(&turn.actor)]);
    }

    // Only those within the hour count, so the older ones are of no more use
    join_attempt::Entity::delete_many()
        .filter(join_attempt::Column::UserId.eq(user_id))
        .filter(join_attempt::Column::At.lte(now - WINDOW))
        .exec(db)
        .await?;
    let wrong = join_attempt::ActiveModel {
        user_id: Set(user_id),
        at: Set(now),
        ..Default::default()
    };
    join_attempt::Entity::insert(wrong).exec(db).await?;
    let wrong = wrong_codes(db, user_id).await?;
    // The code typed is never logged (NFR-PRIV-05)
    info!(wrong = wrong.len(), "wrong join code");
    Ok(vec![match blocked_until(&wrong, now) {
        Some(until) => turn.templates.join_code_blocked(until),
        None => turn.templates.join_code_wrong(MAX_WRONG - wrong.len()),
    }])
}

/// When user `user_id` typed their last [`MAX_WRONG`] wrong join codes, newest first.
async fn wrong_codes(db: &impl ConnectionTrait, user_id: i64) -> Result<Vec<DateTimeUtc>, DbErr> {
    join_attempt::Entity::find()
        .select_only()
        .column(join_attempt::Column::At)
        .filter(join_attempt::Column::UserId.eq(user_id))
        .order_by_desc(join_attempt::Column::At)
        .limit(MAX_WRONG as u64)
        .into_tuple()
        .all(db)
        .await
}

/// Until when join codes are refused at `now`, given when the user's last wrong ones were typed,
/// newest first: for [`BLOCK`] after the last of [`MAX_WRONG`] typed within [`WINDOW`]
/// (FR-ONB-03). `None` while they may try.
fn blocked_until(wrong: &[DateTimeUtc], now: DateTimeUtc) -> Option<DateTimeUtc> {
    let (&newest, &oldest) = (wrong.first()?, wrong.get(MAX_WRONG - 1)?);
    let until = newest + BLOCK;
    (newest - oldest < WINDOW && now < until).then_some(until)
}

/// Whether `typed` is the join `code`, whatever the spaces around it, the case of its Latin
/// letters, or the digits it is typed in: `๑๒๓๔` is `1234`.
fn is_join_code(typed: &str, code: &str) -> bool {
    let normal = |text: &str| thai::arabic_digits(text.trim());
    normal(typed).eq_ignore_ascii_case(&normal(code))
}

/// A number that changes whenever `privacy.noticeVersion` does: 63 bits of its SHA-256, so it
/// travels in postback data, which takes no text (NFR-SEC-03).
fn fingerprint(version: &str) -> i64 {
    let digest = Sha256::digest(version.as_bytes());
    let mut first = [0; 8];
    first.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(first) & i64::MAX
}
