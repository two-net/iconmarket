//! Dispatcher (SRS §2.1): processes inbox events in order per user and routes input
//! (§3.1.3). Built by #13 and #14.
//!
//! The conversation framework every chat feature is built on (#13):
//!
//! - [`Turn`]: what handling one event works with, and the notice of an expired flow;
//! - [`postback`]: postback data, an action name and opaque IDs (FR-UI-13, NFR-SEC-03);
//! - [`flow`]: guided flows, kept per user and expiring when idle (FR-UI-04, FR-UI-15);
//! - [`pending`]: confirmations of proposed changes, applied once (FR-UI-13);
//! - [`menu`]: the main menu for the user's roles (FR-UI-05).
//!
//! Every message that expects an answer offers buttons or Quick Replies for the next step
//! (FR-UI-03), so that every core task can be done by tapping, except typing names, prices,
//! other quantities and addresses (FR-UI-02).

#![allow(
    dead_code,
    reason = "the event loop that hands events to the framework is built by #14"
)]

pub mod flow;
pub mod menu;
pub mod pending;
pub mod postback;
#[cfg(test)]
mod tests;

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

use crate::config::SessionConfig;
use crate::line::Message;
use crate::line::limits::MESSAGES_PER_REQUEST;
use crate::notifications::templates::Templates;
use crate::persistence::entity::{pending_action, session};
use crate::users::access::Actor;

/// How long an expired session is kept, so that a user who comes back the same day is told
/// their step expired (FR-UI-15) instead of finding no trace of it.
const EXPIRED_SESSION_GRACE: TimeDelta = TimeDelta::days(1);

/// What handling one of a user's events works with.
pub struct Turn<'a> {
    pub db: &'a DatabaseConnection,
    pub templates: &'a Templates,
    /// The user as they are now, loaded for each event, so a lost role or a suspension applies
    /// to the very next tap (NFR-SEC-10)
    pub actor: Actor,
    /// When the event is handled, which expiry is judged by
    pub now: DateTimeUtc,
    /// Their guided flow in progress, as the event found it
    pub flow: Option<flow::Stored>,
    session: &'a SessionConfig,
    /// Whether their flow expired since their last event
    expired: bool,
}

impl<'a> Turn<'a> {
    /// Starts handling an event of `actor`'s at `now`, with their flow in progress. A flow left
    /// unanswered for `session.timeoutMinutes` ends here, and [`reply`](Self::reply) says so.
    pub async fn new(
        db: &'a DatabaseConnection,
        templates: &'a Templates,
        session: &'a SessionConfig,
        actor: Actor,
        now: DateTimeUtc,
    ) -> Result<Self, DbErr> {
        let (flow, expired) = match flow::find(db, actor.user.id, now).await? {
            flow::Found::None => (None, false),
            flow::Found::Active(stored) => (Some(stored), false),
            flow::Found::Expired => (None, true),
        };
        Ok(Self {
            db,
            templates,
            actor,
            now,
            flow,
            session,
            expired,
        })
    }

    /// Ends the flow in progress as if it had expired, for one this release can't continue: a
    /// flow renamed or removed, or a draft it can no longer read.
    pub async fn expire_flow(&mut self) -> Result<(), DbErr> {
        if self.flow.take().is_some() {
            flow::end(self.db, self.actor.user.id).await?;
            self.expired = true;
        }
        Ok(())
    }

    /// The reply to the event: `messages`, after the notice that the user's flow expired if it
    /// did (FR-UI-15). The notice is dropped rather than the reply when there is no room for it.
    pub fn reply(&self, mut messages: Vec<Message>) -> Vec<Message> {
        if self.expired && messages.len() < MESSAGES_PER_REQUEST {
            let notice = self.templates.step_expired(self.session.timeout_minutes);
            messages.insert(0, notice);
        }
        messages
    }

    /// When a flow answered now expires, unless answered again (FR-UI-15).
    fn flow_expires_at(&self) -> DateTimeUtc {
        self.now + TimeDelta::minutes(self.session.timeout_minutes.into())
    }
}

/// What [`delete_expired`] deleted.
#[derive(Debug, PartialEq, Eq)]
pub struct Deleted {
    pub sessions: u64,
    pub pending_actions: u64,
}

/// Deletes the sessions that expired more than [`EXPIRED_SESSION_GRACE`] before `now`, and the
/// pending actions that expired before `now`, consumed or not (§5.4). A tap on a deleted
/// action's button finds nothing, so it shows the current state (FR-UI-13).
pub async fn delete_expired(db: &impl ConnectionTrait, now: DateTimeUtc) -> Result<Deleted, DbErr> {
    let sessions = session::Entity::delete_many()
        .filter(session::Column::ExpiresAt.lt(now - EXPIRED_SESSION_GRACE))
        .exec(db)
        .await?
        .rows_affected;
    let pending_actions = pending_action::Entity::delete_many()
        .filter(pending_action::Column::ExpiresAt.lt(now))
        .exec(db)
        .await?
        .rows_affected;
    Ok(Deleted {
        sessions,
        pending_actions,
    })
}
