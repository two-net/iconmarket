//! Sending a payment slip (FR-PAY-05, FR-UI-14): the buyer's photo of their PromptPay transfer,
//! for the seller to confirm.
//!
//! The buyer of an accepted order that [awaits a transfer](payments::slip::awaits_transfer)
//! taps "ส่งสลิป", on the notice telling them what to transfer where or on the order's card, and
//! sends the photo. A photo they send unasked, which no flow takes, is asked about instead: is
//! it the slip of that order? Either way the photo is downloaded as it arrives (FR-LINE-09) and
//! kept as their draft until it is sent; one they say isn't the slip is deleted.
//!
//! Sent, the slip goes to the seller as an image behind its expiring link (AC-14), with
//! "ได้รับเงินแล้ว" and "ยังไม่ได้รับ", and the order awaits their confirmation. Those answers are
//! [acting on the order](super::actions): the first records it paid; the second refuses the slip
//! and tells the buyer.

#[cfg(test)]
mod tests;

use sea_orm::prelude::Uuid;
use sea_orm::{DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::actions::{self, Parties};
use super::{MARK_PAID, NOT_PAID, ORDER_ID, SLIP_ID, contact};
use crate::catalog::images::{self, Owner, Rejected, SlipLink};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{self, Message};
use crate::notifications::templates::labels;
use crate::notifications::{Kind, To};
use crate::payments::{self, slip::Submitted};
use crate::persistence::entity::order;
use crate::persistence::entity::order_event::ActorType;
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// The flow sending the slip of an order (FR-PAY-05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlipFlow {
    order_id: i64,
    /// The photo sent unasked, the buyer's draft, while they are asked whether it is the slip
    photo: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// The photo of the slip, asked for by "ส่งสลิป"
    Photo,
    /// Whether the photo sent unasked is the slip (FR-UI-14)
    Confirm,
}

impl Flow for SlipFlow {
    const NAME: &'static str = "payment_slip";
    type Step = Step;
}

/// The buyer's "ส่งสลิป" on order `id` (FR-PAY-05): asks for the photo of the slip while the
/// order awaits a transfer, and shows it as it is otherwise.
pub(super) async fn start(turn: &Turn<'_>, id: i64) -> Result<Vec<Message>, DbErr> {
    let order = match buyers(turn, id).await? {
        Ok(order) => order,
        Err(refused) => return Ok(vec![refused]),
    };
    if !payments::slip::awaits_transfer(&order) {
        let now = actions::paid_now_is(turn.templates, &order, ActorType::Buyer);
        return Ok(vec![now]);
    }
    let ask = vec![ask(turn, &order, Step::Photo)];
    let draft = SlipFlow {
        order_id: id,
        photo: None,
    };
    let session = Session {
        step: Step::Photo,
        draft,
    };
    flow::start(turn, session, ask).await
}

/// A photo no flow took, sent in `message_id` by the buyer of `order`, which awaits a transfer
/// (FR-UI-14 step 3): asks whether it is the slip of that order. Until they say, it is kept as
/// their draft.
pub async fn photo_sent(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    message_id: &str,
    order: &order::Model,
) -> Result<Vec<Message>, DbErr> {
    let access = access::Action::ActAsBuyer(order.id);
    if let Err(denied) = authorize(turn.db, &turn.actor, access).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let photo = match receive(turn, config, line, message_id).await? {
        Ok(photo) => photo,
        Err(rejected) => {
            let rejected = Message::text(turn.templates.image_rejected(rejected));
            return Ok(vec![rejected, menu::main_menu(&turn.actor)]);
        }
    };
    let draft = SlipFlow {
        order_id: order.id,
        photo: Some(photo),
    };
    let session = Session {
        step: Step::Confirm,
        draft,
    };
    flow::start(turn, session, vec![ask(turn, order, Step::Confirm)]).await
}

/// The answer to the step the buyer is on (FR-UI-12 step 3): the photo of the slip, or whether
/// the photo they sent is the slip; another photo then is asked about instead. `None` for a
/// location, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    session: Session<SlipFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Session { step, draft } = session;
    // A suspension mid-flow ends it (NFR-SEC-10), as does the order no longer awaiting a
    // transfer: cancelled, say
    let ended = match buyers(turn, draft.order_id).await? {
        Ok(order) if payments::slip::awaits_transfer(&order) => Ok(order),
        Ok(order) => Err(actions::paid_now_is(
            turn.templates,
            &order,
            ActorType::Buyer,
        )),
        Err(refused) => Err(refused),
    };
    let order = match ended {
        Ok(order) => order,
        Err(ended) => {
            discard(turn, draft.photo).await?;
            let done = Answer::<SlipFlow>::Done(vec![ended]);
            return flow::answered(turn, done).await.map(Some);
        }
    };
    let is = |text: &str, label: &Label| text.trim() == label.as_str();
    let messages = match (input, step, draft.photo) {
        (Input::Image { message_id }, ..) => {
            let photo = match receive(turn, config, line, message_id).await? {
                Ok(photo) => photo,
                Err(rejected) => {
                    let rejected = Message::text(turn.templates.image_rejected(rejected));
                    let again = vec![rejected, ask(turn, &order, step)];
                    let again = Answer::<SlipFlow>::Again(again);
                    return flow::answered(turn, again).await.map(Some);
                }
            };
            match step {
                Step::Photo => send(turn, config, &order, photo).await?,
                Step::Confirm => {
                    discard(turn, draft.photo).await?;
                    let draft = SlipFlow {
                        photo: Some(photo),
                        ..draft
                    };
                    let ask = vec![ask(turn, &order, step)];
                    flow::answered(turn, Answer::Next(Session { step, draft }, ask)).await?
                }
            }
        }
        (Input::Text(text), Step::Confirm, Some(photo)) if is(text, &labels::YES) => {
            send(turn, config, &order, photo).await?
        }
        (Input::Text(text), Step::Confirm, photo) if is(text, &labels::NOT_SLIP) => {
            discard(turn, photo).await?;
            let done = vec![turn.templates.not_a_slip(), menu::main_menu(&turn.actor)];
            flow::answered(turn, Answer::<SlipFlow>::Done(done)).await?
        }
        (Input::Text(_), ..) => {
            let again = Answer::<SlipFlow>::Again(vec![ask(turn, &order, step)]);
            flow::answered(turn, again).await?
        }
        _ => return Ok(None),
    };
    Ok(Some(messages))
}

/// What asks for `step` of sending the slip of `order`.
fn ask(turn: &Turn<'_>, order: &order::Model, step: Step) -> Message {
    let (number, total) = (&order.order_number, order.total);
    match step {
        Step::Photo => turn.templates.ask_slip(number, total),
        Step::Confirm => turn.templates.is_it_the_slip(number, total),
    }
}

/// Order `id`, if the actor may send its slip: as its buyer, not suspended (NFR-SEC-02).
/// Otherwise why not.
async fn buyers(turn: &Turn<'_>, id: i64) -> Result<Result<order::Model, Message>, DbErr> {
    let access = access::Action::ActAsBuyer(id);
    if let Err(denied) = authorize(turn.db, &turn.actor, access).await? {
        return Ok(Err(turn.denied(denied)));
    }
    Ok(match order::Entity::find_by_id(id).one(turn.db).await? {
        Some(order) => Ok(order),
        None => Err(turn.templates.denied(Denied::NotFound)),
    })
}

/// Sends `photo`, the buyer's draft, as the slip of `order` (FR-PAY-05): pushed to the seller as
/// an image whose link lasts `image.slipLinkHours` (AC-14), above the notice with
/// "ได้รับเงินแล้ว" and "ยังไม่ได้รับ", which is the preview the chat list shows. The flow ends,
/// unless the photo is gone: then it asks for one again.
async fn send(
    turn: &Turn<'_>,
    config: &Config,
    order: &order::Model,
    photo: Uuid,
) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let link = SlipLink::new(turn.now, &config.image);
    let buyer = turn.actor.user.id;
    let submitted = payments::slip::submit(turn.db, order.id, buyer, photo, link, turn.now).await?;
    let (order, slip) = match submitted {
        Submitted::Done { order, slip } => (order, slip),
        Submitted::Refused(order) => {
            discard(turn, Some(photo)).await?;
            let now = actions::paid_now_is(templates, &order, ActorType::Buyer);
            return flow::answered(turn, Answer::<SlipFlow>::Done(vec![now])).await;
        }
        // Swept, its flow having been idle too long: sent again, it is kept
        Submitted::Gone => {
            let gone = Message::text(templates.image_rejected(Rejected::Unavailable));
            let draft = SlipFlow {
                order_id: order.id,
                photo: None,
            };
            let session = Session {
                step: Step::Photo,
                draft,
            };
            let ask = vec![gone, ask(turn, order, Step::Photo)];
            return flow::answered(turn, Answer::Next(session, ask)).await;
        }
        Submitted::NotFound => {
            let gone = vec![templates.denied(Denied::NotFound)];
            return flow::answered(turn, Answer::<SlipFlow>::Done(gone)).await;
        }
    };
    let parties = Parties::of(turn.db, &order).await?;
    let url = images::slip_image_url(&config.app.base_url, slip.access_token);
    let answers = vec![
        Postback::new(MARK_PAID)
            .with_int(ORDER_ID, order.id)
            .button(labels::MARK_PAID.clone()),
        Postback::new(NOT_PAID)
            .with_int(ORDER_ID, order.id)
            .with_int(SLIP_ID, slip.id)
            .button(labels::NOT_PAID.clone()),
    ];
    let buyer = contact::buyer_name(&parties.buyer);
    let hours = config.image.slip_link_hours;
    let notice =
        templates.slip_received_notice(&order.order_number, order.total, buyer, hours, answers);
    let seller = To::User(parties.shop.owner_id);
    turn.notify(
        Kind::SlipReceived,
        seller,
        vec![Message::image(&url, &url), notice],
    );
    turn.essential();
    let next = actions::buttons(&order, ActorType::Buyer);
    let sent = templates.slip_sent(&order.order_number, next);
    flow::answered(turn, Answer::<SlipFlow>::Done(vec![sent])).await
}

/// Downloads the photo the buyer sent in `message_id` (FR-LINE-09) and keeps it as their draft.
async fn receive(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    message_id: &str,
) -> Result<Result<Uuid, Rejected>, DbErr> {
    let processed = match images::receive(line, message_id, &config.image).await {
        Ok(Ok(processed)) => processed,
        Ok(Err(rejected)) => return Ok(Err(rejected)),
        Err(e) => {
            warn!(error = %e, "cannot download a payment slip");
            return Ok(Err(Rejected::Unavailable));
        }
    };
    let owner = Owner::Draft(turn.actor.user.id);
    images::store(turn.db, owner, processed, &config.image, turn.now).await
}

/// Deletes `photo`, the buyer's draft, if there is one: not the slip after all, or not to be
/// sent. A slip shows the payer's name and account, so it isn't left for the sweep.
async fn discard(turn: &Turn<'_>, photo: Option<Uuid>) -> Result<(), DbErr> {
    match photo {
        Some(photo) => images::delete_draft(turn.db, photo, turn.actor.user.id).await,
        None => Ok(()),
    }
}
