//! Guided flows (FR-UI-15): a user's way through a series of steps, kept in `sessions`, one per
//! user, so it survives a restart (NFR-REL-03). It holds which flow, the step it waits on and
//! the draft entered so far. It ends when the flow finishes or the user cancels (FR-UI-04), or
//! after `session.timeoutMinutes` without an answer; the reply to the user's next event then
//! says so, once ([`Turn::new`]).
//!
//! A flow is a type implementing [`Flow`], in the module that owns what the flow makes. That
//! module answers the flow's steps: the dispatcher finds it by [`Stored::flow`] and hands it
//! the [`Input`](super::Input), and [`answered`] stores what the step made of it.

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, TransactionTrait};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{Turn, menu, pending};
use crate::line::Message;
use crate::notifications::templates::{self, labels};
use crate::persistence::entity::session;

/// A guided flow. A value of the type is its draft, kept as JSON in `sessions.data`.
pub trait Flow: Serialize + DeserializeOwned {
    /// Its name in `sessions.flow`. Keep it across releases, or flows in progress at an upgrade
    /// end as if expired.
    const NAME: &'static str;

    /// Its steps: an enum without fields, kept by variant name in `sessions.step`
    type Step: Copy + Serialize + DeserializeOwned;
}

/// A flow in progress: the step it waits on, and the draft so far.
#[derive(Clone, Debug, PartialEq)]
pub struct Session<F: Flow> {
    pub step: F::Step,
    pub draft: F,
}

/// A user's flow in progress as stored, for the module that owns it to read.
#[derive(Clone, Debug, PartialEq)]
pub struct Stored(session::Model);

impl Stored {
    /// The flow's [`Flow::NAME`].
    pub fn flow(&self) -> &str {
        &self.0.flow
    }

    /// It as flow `F`; `None` if it is another flow, or one whose step or draft this release
    /// can't read. The dispatcher ends such a flow with
    /// [`Turn::expire_flow`](super::Turn::expire_flow).
    pub fn read<F: Flow>(&self) -> Option<Session<F>> {
        if self.0.flow != F::NAME {
            return None;
        }
        let step = serde_json::from_value(Json::String(self.0.step.clone())).ok()?;
        let draft = serde_json::from_value(self.0.data.clone()).ok()?;
        Some(Session { step, draft })
    }
}

/// A user's flow as an event of theirs finds it.
pub(super) enum Found {
    None,
    Active(Stored),
    /// Unanswered for `session.timeoutMinutes`; now deleted, so it is reported once
    Expired,
}

/// User `user_id`'s flow at `now`. An expired one is deleted as it is found.
pub(super) async fn find(
    db: &impl ConnectionTrait,
    user_id: i64,
    now: DateTimeUtc,
) -> Result<Found, DbErr> {
    Ok(match session::Entity::find_by_id(user_id).one(db).await? {
        None => Found::None,
        Some(row) if row.expires_at > now => Found::Active(Stored(row)),
        Some(_) => {
            end(db, user_id).await?;
            Found::Expired
        }
    })
}

/// What a step made of the user's answer.
pub enum Answer<F: Flow> {
    /// On to the step `ask` asks for
    Next(Session<F>, Vec<Message>),
    /// Not an answer the step takes: `ask` says what to enter instead, and the step waits again
    Again(Vec<Message>),
    /// The flow is over; the messages say how it ended
    Done(Vec<Message>),
}

/// Stores what a step of flow `F` made of the user's answer, and returns the reply. Every answer
/// counts as activity, so the timeout starts again. The last message of a prompt gets the cancel
/// button, so every step can be left (FR-UI-03, FR-UI-04).
pub async fn answered<F: Flow>(turn: &Turn<'_>, answer: Answer<F>) -> Result<Vec<Message>, DbErr> {
    let user_id = turn.actor.user.id;
    match answer {
        Answer::Next(session, ask) => {
            save(turn.db, user_id, &session, turn.flow_expires_at()).await?;
            Ok(prompt(ask))
        }
        Answer::Again(ask) => {
            session::Entity::update_many()
                .col_expr(
                    session::Column::ExpiresAt,
                    Expr::value(turn.flow_expires_at()),
                )
                .filter(session::Column::UserId.eq(user_id))
                .exec(turn.db)
                .await?;
            Ok(prompt(ask))
        }
        Answer::Done(messages) => {
            end(turn.db, user_id).await?;
            Ok(messages)
        }
    }
}

/// Starts flow `F` at the step `ask` asks for, in place of any flow in progress.
pub async fn start<F: Flow>(
    turn: &Turn<'_>,
    session: Session<F>,
    ask: Vec<Message>,
) -> Result<Vec<Message>, DbErr> {
    answered(turn, Answer::Next(session, ask)).await
}

/// Ends user `user_id`'s flow, if any, keeping nothing of it. Returns whether there was one.
pub async fn end(db: &impl ConnectionTrait, user_id: i64) -> Result<bool, DbErr> {
    let deleted = session::Entity::delete_by_id(user_id).exec(db).await?;
    Ok(deleted.rows_affected > 0)
}

/// Whether `text` is `ยกเลิก`, which always cancels (FR-UI-04, FR-UI-12).
pub fn is_cancel(text: &str) -> bool {
    text.trim() == labels::CANCEL.as_str()
}

/// `ยกเลิก`, typed or tapped (FR-UI-04): ends the flow in progress, discarding its draft, and
/// [discards](pending::discard) every change the user has yet to confirm, then shows the main
/// menu. `turn.flow` stays as the event found it.
pub async fn cancel(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    let user_id = turn.actor.user.id;
    let txn = turn.db.begin().await?;
    let ended = end(&txn, user_id).await?;
    let discarded = pending::discard(&txn, user_id).await?;
    txn.commit().await?;

    let menu = menu::main_menu(&turn.actor);
    Ok(if ended || discarded > 0 {
        vec![turn.templates.cancelled(), menu]
    } else {
        vec![menu]
    })
}

/// Stores `session` as user `user_id`'s flow, in place of any other, until `expires_at`.
async fn save<F: Flow>(
    db: &impl ConnectionTrait,
    user_id: i64,
    session: &Session<F>,
    expires_at: DateTimeUtc,
) -> Result<(), DbErr> {
    let step = match serde_json::to_value(session.step) {
        Ok(Json::String(step)) => step,
        _ => panic!(
            "the steps of flow {} are not an enum without fields",
            F::NAME
        ),
    };
    let data = serde_json::to_value(&session.draft).map_err(|e| DbErr::Json(e.to_string()))?;
    let row = session::ActiveModel {
        user_id: Set(user_id),
        flow: Set(F::NAME.to_owned()),
        step: Set(step),
        data: Set(data),
        expires_at: Set(expires_at),
    };
    session::Entity::insert(row)
        .on_conflict(
            OnConflict::column(session::Column::UserId)
                .update_columns([
                    session::Column::Flow,
                    session::Column::Step,
                    session::Column::Data,
                    session::Column::ExpiresAt,
                ])
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    Ok(())
}

/// `ask` with the cancel button on its last message.
fn prompt(mut ask: Vec<Message>) -> Vec<Message> {
    debug_assert!(!ask.is_empty(), "a step asks for something");
    if let Some(last) = ask.pop() {
        ask.push(templates::with_cancel(last));
    }
    ask
}
