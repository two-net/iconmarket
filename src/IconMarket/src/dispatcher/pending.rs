//! Confirmations (FR-UI-13): a change proposed to the user, such as an AI proposal (FR-AI-03),
//! a checkout summary or an edit, waits on the server as a pending action until they tap its
//! confirm button. The button carries only the action's random ID (NFR-SEC-04), never the values
//! proposed (NFR-SEC-03).
//!
//! A tap goes through [`confirm`], which applies the change at most once, and only after
//! re-checking the user's authorization and what the change acts on. A tap that comes too late,
//! or again, changes nothing and shows the current state instead. Pending actions last
//! `session.pendingActionMinutes`, consumed or not, and are then deleted
//! ([`delete_expired`](super::delete_expired)).
//!
//! A kind of proposal is a type implementing [`Proposal`], in the module that owns what it
//! changes, with an arm in [`tapped`].
//!
//! A value the user gave that a button must come back to without changing anything, such as the
//! search that "ดูเพิ่มเติม" pages, is [`keep`]: a pending action that is never consumed, so its
//! buttons work, as often as tapped, until it expires. Like a proposal, `ยกเลิก` discards it.

use std::future::Future;

use chrono::TimeDelta;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter,
    TransactionTrait,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use super::Turn;
use super::postback::Postback;
use crate::admin::categories::DeleteCategory;
use crate::catalog::products::manage::{DeleteProduct, UpdateProduct};
use crate::line::Message;
use crate::orders::cart::AddToCart;
use crate::orders::checkout::Checkout;
use crate::persistence::entity::pending_action;
use crate::persistence::random_id;
use crate::users::access::{self, Denied, authorize};
use crate::users::deletion::DeleteAccount;

/// The action of every confirm button
pub const CONFIRM: &str = "confirm";
/// Its parameters: the proposal's kind, and the pending action's ID
const KIND: &str = "k";
const ID: &str = "pa";

/// A kind of change proposed for confirmation. A value of the type is the proposal: what the
/// user is asked to confirm, kept as JSON in `pending_actions.payload`. It holds IDs and the
/// values proposed, never personal data.
pub trait Proposal: Serialize + DeserializeOwned + Send + Sync {
    /// Its name in `pending_actions.kind` and in its confirm button. Keep it across releases,
    /// or buttons sent before an upgrade find nothing.
    const KIND: &'static str;

    /// What the user must be allowed to do for it to apply, checked at the tap (NFR-SEC-02).
    fn requires(&self) -> access::Action;

    /// Applies it in `txn`, the pending action `id` having just been consumed there: everything
    /// it changes goes through `txn`, a savepoint of the event's transaction that is kept only
    /// on [`Applied::Done`]. Before changing
    /// anything it re-checks that what it acts on still exists, in the state and at the prices
    /// proposed (FR-UI-13); if not, it returns [`Applied::Changed`].
    fn apply(
        &self,
        id: Uuid,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
    ) -> impl Future<Output = Result<Applied, DbErr>> + Send;

    /// The current state of what it acts on, shown instead of acting. Runs after the savepoint
    /// is undone, so it may [`propose`] again, e.g. an updated summary.
    fn current_state(
        stale: Stale<Self>,
        turn: &Turn<'_>,
    ) -> impl Future<Output = Result<Vec<Message>, DbErr>> + Send;
}

/// What [`Proposal::apply`] did.
#[derive(Debug)]
pub enum Applied {
    /// Applied; the messages say so
    Done(Vec<Message>),
    /// What it acts on changed since it was proposed, so nothing is applied
    Changed,
}

/// A confirmation that doesn't apply, for [`Proposal::current_state`].
#[derive(Debug)]
pub struct Stale<P> {
    pub why: Why,
    /// The pending action tapped
    pub id: Uuid,
    /// The proposal, while it is stored
    pub proposal: Option<P>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// An earlier tap applied it
    Confirmed,
    /// Expired, cancelled, someone else's, or never there
    Gone,
    /// What it acts on changed
    Changed,
}

/// Stores `proposal` for the user to confirm within `session.pendingActionMinutes`, and returns
/// the postback for its confirm button, which the caller labels.
pub async fn propose<P: Proposal>(turn: &Turn<'_>, proposal: &P) -> Result<Postback, DbErr> {
    let id = keep(turn, P::KIND, proposal).await?;
    Ok(Postback::new(CONFIRM)
        .with_name(KIND, P::KIND)
        .with_uuid(ID, id))
}

/// Stores `value` as `kind` for the user's buttons to come back to within
/// `session.pendingActionMinutes`, and returns its random ID, for them to carry. Unless a
/// proposal confirms it, it is never consumed: [`kept`] reads it as often as asked.
pub async fn keep<T: Serialize>(
    turn: &Turn<'_>,
    kind: &'static str,
    value: &T,
) -> Result<Uuid, DbErr> {
    let id = random_id();
    let payload = serde_json::to_value(value).map_err(|e| DbErr::Json(e.to_string()))?;
    let lifetime = TimeDelta::minutes(turn.session.pending_action_minutes.into());
    let row = pending_action::ActiveModel {
        id: Set(id),
        user_id: Set(turn.actor.user.id),
        kind: Set(kind.to_owned()),
        payload: Set(payload),
        expires_at: Set(turn.now + lifetime),
        consumed_at: Set(None),
        created_at: Set(turn.now),
    };
    pending_action::Entity::insert(row)
        .exec_without_returning(turn.db)
        .await?;
    Ok(id)
}

/// What the user [`keep`]s as `kind` under `id`, while it lasts. `None` once it expired or was
/// discarded, if it is someone else's or another kind, or if this release can't read it.
pub async fn kept<T: DeserializeOwned>(
    turn: &Turn<'_>,
    kind: &'static str,
    id: Uuid,
) -> Result<Option<T>, DbErr> {
    let row = pending_action::Entity::find_by_id(id)
        .filter(pending_action::Column::UserId.eq(turn.actor.user.id))
        .filter(pending_action::Column::Kind.eq(kind))
        .filter(pending_action::Column::ExpiresAt.gt(turn.now))
        .one(turn.db)
        .await?;
    Ok(row.and_then(|row| {
        serde_json::from_value(row.payload)
            .inspect_err(|e| warn!(kind, error = %e, "cannot read a kept value"))
            .ok()
    }))
}

/// A tap on a confirm button: confirms its pending action as the kind it names. Each kind of
/// proposal adds its arm, as its issue builds it: deleting a category (#21), deleting a product
/// (#26), quick updates to a product (#30), checkout (#32), deleting the account (#36) and the
/// AI assistant's proposals (#37). A product being listed is confirmed by its preview's
/// revision instead (#25, #30), and an order's buttons, which carry no proposed values, by the
/// order's status (#33).
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    match (postback.name(KIND), postback.uuid(ID)) {
        (Some(DeleteCategory::KIND), Some(id)) => confirm::<DeleteCategory>(turn, id).await,
        (Some(DeleteProduct::KIND), Some(id)) => confirm::<DeleteProduct>(turn, id).await,
        (Some(UpdateProduct::KIND), Some(id)) => confirm::<UpdateProduct>(turn, id).await,
        (Some(Checkout::KIND), Some(id)) => confirm::<Checkout>(turn, id).await,
        (Some(AddToCart::KIND), Some(id)) => confirm::<AddToCart>(turn, id).await,
        (Some(DeleteAccount::KIND), Some(id)) => confirm::<DeleteAccount>(turn, id).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// Confirms pending action `id` as a `P` (FR-UI-13). In one savepoint it consumes the action,
/// checks the user's authorization and applies the proposal, so that it applies at most once
/// however often or however concurrently it is tapped, and not at all if anything fails or
/// changed. Otherwise the reply is the current state.
pub async fn confirm<P: Proposal>(turn: &Turn<'_>, id: Uuid) -> Result<Vec<Message>, DbErr> {
    let user_id = turn.actor.user.id;
    let txn = turn.db.begin().await?;
    // A second tap waits here until the first's event commits, then finds the action consumed
    let taken = pending_action::Entity::update_many()
        .col_expr(pending_action::Column::ConsumedAt, Expr::value(turn.now))
        .filter(pending_action::Column::Id.eq(id))
        .filter(pending_action::Column::UserId.eq(user_id))
        .filter(pending_action::Column::Kind.eq(P::KIND))
        .filter(pending_action::Column::ConsumedAt.is_null())
        .filter(pending_action::Column::ExpiresAt.gt(turn.now))
        .exec_with_returning(&txn)
        .await?;
    let Some(row) = taken.first() else {
        txn.rollback().await?;
        return stale::<P>(turn, id).await;
    };
    let Some(proposal) = read::<P>(row) else {
        txn.rollback().await?;
        let stale = Stale {
            why: Why::Gone,
            id,
            proposal: None,
        };
        return P::current_state(stale, turn).await;
    };

    if let Err(denied) = authorize(&txn, &turn.actor, proposal.requires()).await? {
        txn.rollback().await?;
        return Ok(vec![turn.denied(denied)]);
    }
    match proposal.apply(id, &txn, turn).await? {
        Applied::Done(messages) => {
            txn.commit().await?;
            Ok(messages)
        }
        Applied::Changed => {
            txn.rollback().await?;
            let stale = Stale {
                why: Why::Changed,
                id,
                proposal: Some(proposal),
            };
            P::current_state(stale, turn).await
        }
    }
}

/// Deletes the changes user `user_id` has yet to confirm, and the values [kept](keep) for their
/// buttons, so those buttons show the current state (FR-UI-04). Confirmed ones are kept until
/// they expire, so a repeated tap is still recognised. Returns how many were deleted.
pub async fn discard(db: &impl ConnectionTrait, user_id: i64) -> Result<u64, DbErr> {
    let deleted = pending_action::Entity::delete_many()
        .filter(pending_action::Column::UserId.eq(user_id))
        .filter(pending_action::Column::ConsumedAt.is_null())
        .exec(db)
        .await?;
    Ok(deleted.rows_affected)
}

/// The current state, for a tap on pending action `id` that consumed nothing.
async fn stale<P: Proposal>(turn: &Turn<'_>, id: Uuid) -> Result<Vec<Message>, DbErr> {
    // Only the user's own, of this kind: anything else is as good as never there
    let row = pending_action::Entity::find_by_id(id)
        .filter(pending_action::Column::UserId.eq(turn.actor.user.id))
        .filter(pending_action::Column::Kind.eq(P::KIND))
        .one(turn.db)
        .await?;
    let (why, proposal) = match row {
        Some(row) if row.consumed_at.is_some() => (Why::Confirmed, read(&row)),
        // Expired, and not deleted yet
        Some(row) => (Why::Gone, read(&row)),
        None => (Why::Gone, None),
    };
    P::current_state(Stale { why, id, proposal }, turn).await
}

/// The proposal a pending action holds; `None` if this release can't read it.
fn read<P: Proposal>(row: &pending_action::Model) -> Option<P> {
    serde_json::from_value(row.payload.clone())
        .inspect_err(|e| warn!(kind = P::KIND, error = %e, "cannot read a pending action"))
        .ok()
}
