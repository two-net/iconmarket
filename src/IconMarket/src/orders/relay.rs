//! Relaying messages (FR-ORD-13): while an order is active, its buyer and its seller send each
//! other short texts through the OA.
//!
//! The buyer taps "ส่งข้อความถึงร้าน", the seller "ส่งข้อความถึงลูกค้า", on a card or notice of
//! the order while it is active, and types the message: one message a tap. It is pushed to the
//! other party led by the order's number, with the button to answer back, and logged as every
//! push is (FR-NOT-05), so each one counts against the OA's monthly quota.
//!
//! Before an order is accepted and once it is over, nothing is relayed: the buttons aren't
//! offered, an old one shows the order as it is (FR-UI-13), and a message typed as the order
//! moved on isn't sent. Nor is one to a party who doesn't follow the OA, whom no push reaches
//! (FR-NOT-02); the sender is told so instead. The message itself is never logged
//! (NFR-PRIV-05).

#[cfg(test)]
mod tests;

use sea_orm::{ConnectionTrait, DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::actions::{self, Parties};
use super::{MESSAGE_BUYER, MESSAGE_SHOP, ORDER_ID, VIEW, contact};
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message};
use crate::notifications::templates::{OrderParty, labels};
use crate::notifications::{self, Kind, To};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{order, user};
use crate::users::access::{self, Denied, authorize};

/// The longest message, in characters, as for a reason or a report
pub const MESSAGE_CHARS: usize = 300;

/// The flow asking for the message to relay (FR-ORD-13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayFlow {
    order_id: i64,
    from: Side,
}

/// A party of an order, sending a message or getting one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// Its buyer
    Buyer,
    /// Its shop's owner
    Seller,
}

impl Side {
    /// As whom the party acts on the order.
    fn actor(self) -> ActorType {
        match self {
            Self::Buyer => ActorType::Buyer,
            Self::Seller => ActorType::Seller,
        }
    }

    /// The party a message from this one goes to.
    fn other(self) -> Self {
        match self {
            Self::Buyer => Self::Seller,
            Self::Seller => Self::Buyer,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// The message, typed
    Text,
}

impl Flow for RelayFlow {
    const NAME: &'static str = "order_message";
    type Step = Step;
}

/// The button of party `from` of `order` that sends the other party a message (FR-ORD-13): the
/// buyer's "ส่งข้อความถึงร้าน", or the seller's "ส่งข้อความถึงลูกค้า".
pub(super) fn button(order: &order::Model, from: Side) -> Action {
    let (action, label) = match from {
        Side::Buyer => (MESSAGE_SHOP, &labels::MESSAGE_SHOP),
        Side::Seller => (MESSAGE_BUYER, &labels::MESSAGE_BUYER),
    };
    Postback::new(action)
        .with_int(ORDER_ID, order.id)
        .button(label.clone())
}

/// Party `from`'s "ส่งข้อความถึงร้าน" or "ส่งข้อความถึงลูกค้า" on order `id` (FR-ORD-13): asks
/// for the message while the order is active and a push reaches the other party. Shows the
/// order as it is otherwise, or says the other party can't be reached.
pub(super) async fn start(turn: &Turn<'_>, id: i64, from: Side) -> Result<Vec<Message>, DbErr> {
    let order = match relayable(turn, id, from).await? {
        Ok(order) => order,
        Err(refused) => return Ok(vec![refused]),
    };
    let parties = Parties::of(turn.db, &order).await?;
    let (number, to) = (&order.order_number, from.other());
    if !reaches(turn.db, &parties, to).await? {
        return Ok(vec![unreachable(turn, &order, &parties, from)]);
    }
    let ask = turn
        .templates
        .ask_message(number, party(&parties, to), MESSAGE_CHARS);
    let session = Session {
        step: Step::Text,
        draft: RelayFlow { order_id: id, from },
    };
    flow::start(turn, session, vec![ask]).await
}

/// The message, typed (FR-UI-12 step 3): pushed to the other party, led by the order's number,
/// with the button to answer back. Not sent if the order is no longer active, the sender may no
/// longer act on it (NFR-SEC-10), or no push reaches the other party any more. `None` for
/// anything but text, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    session: Session<RelayFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let done = |messages| async move {
        flow::answered(turn, Answer::<RelayFlow>::Done(messages))
            .await
            .map(Some)
    };
    let RelayFlow { order_id, from } = session.draft;
    let order = match relayable(turn, order_id, from).await? {
        Ok(order) => order,
        Err(ended) => return done(vec![ended]).await,
    };
    let parties = Parties::of(turn.db, &order).await?;
    let (templates, number, to) = (turn.templates, &order.order_number, from.other());
    if !reaches(turn.db, &parties, to).await? {
        return done(vec![unreachable(turn, &order, &parties, from)]).await;
    }
    let text = text.trim();
    let again = match text.chars().count() {
        0 => Some(templates.ask_message(number, party(&parties, to), MESSAGE_CHARS)),
        chars if chars > MESSAGE_CHARS => Some(templates.text_too_long(MESSAGE_CHARS)),
        _ => None,
    };
    if let Some(again) = again {
        let again = Answer::<RelayFlow>::Again(vec![again]);
        return flow::answered(turn, again).await.map(Some);
    }
    let recipient = match to {
        Side::Buyer => order.buyer_id,
        Side::Seller => parties.shop.owner_id,
    };
    let answer_back = vec![
        button(&order, to),
        Postback::new(VIEW)
            .with_int(ORDER_ID, order.id)
            .button(labels::ORDER_DETAILS.clone()),
    ];
    let notice = templates.relayed_message_notice(number, party(&parties, from), text, answer_back);
    turn.notify(Kind::OrderMessage, To::User(recipient), vec![notice]);
    info!(order_id, "message relayed");
    turn.essential();
    let next = actions::buttons(&order, from.actor());
    done(vec![templates.message_relayed(
        number,
        party(&parties, to),
        next,
    )])
    .await
}

/// Order `id`, if the actor may send a message on it as party `from` now: as its buyer, or as
/// its shop's owner (NFR-SEC-02), while it is active. Otherwise why not, or the order as it is.
async fn relayable(
    turn: &Turn<'_>,
    id: i64,
    from: Side,
) -> Result<Result<order::Model, Message>, DbErr> {
    let access = match from {
        Side::Buyer => access::Action::ActAsBuyer(id),
        Side::Seller => access::Action::HandleOrder(id),
    };
    if let Err(denied) = authorize(turn.db, &turn.actor, access).await? {
        return Ok(Err(turn.denied(denied)));
    }
    let Some(order) = order::Entity::find_by_id(id).one(turn.db).await? else {
        return Ok(Err(turn.templates.denied(Denied::NotFound)));
    };
    if !order.status.is_active() {
        return Ok(Err(actions::now_is(turn.templates, &order, from.actor())));
    }
    Ok(Ok(order))
}

/// Party `side` of the order between `parties`, as the other party calls them.
fn party(parties: &Parties, side: Side) -> OrderParty<'_> {
    match side {
        Side::Buyer => OrderParty::Buyer(contact::buyer_name(&parties.buyer)),
        Side::Seller => OrderParty::Shop(&parties.shop.name),
    }
}

/// Whether a push reaches party `side` of the order between `parties` (FR-NOT-02).
async fn reaches(db: &impl ConnectionTrait, parties: &Parties, side: Side) -> Result<bool, DbErr> {
    let user = match side {
        Side::Buyer => return Ok(notifications::reaches(&parties.buyer)),
        Side::Seller => {
            user::Entity::find_by_id(parties.shop.owner_id)
                .one(db)
                .await?
        }
    };
    Ok(user.is_some_and(|user| notifications::reaches(&user)))
}

/// No message from party `from` of `order`, between `parties`, can reach the other party: what
/// the sender can do instead.
fn unreachable(turn: &Turn<'_>, order: &order::Model, parties: &Parties, from: Side) -> Message {
    let again = button(order, from);
    let instead = actions::buttons(order, from.actor())
        .into_iter()
        .filter(|action| *action != again)
        .collect();
    let to = party(parties, from.other());
    turn.templates
        .message_unreachable(&order.order_number, to, instead)
}
