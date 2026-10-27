//! Checkout (FR-CART-04 – FR-CART-08): a shop's lines of the buyer's cart become one order.
//!
//! "สั่งซื้อเลย" or "สั่งซื้อร้านนี้" starts a guided [flow](CheckoutFlow) that asks, as in
//! Appendix A.2: pickup or delivery, only among what the shop offers; for delivery, whether to
//! deliver to the saved address, a new one or a shared location; the payment method, among
//! those `payment.methods` allows that the shop takes (PromptPay only with its PromptPay ID); and
//! a note for the shop of at most [`NOTE_CHARS`]. A step with one choice is skipped. Each answer
//! is a Quick Reply sent as typed, and the address, if the buyer has none yet, is asked for
//! first (FR-ONB-04).
//!
//! The flow ends with a summary of what would be ordered: items, subtotal, delivery fee, total,
//! how it is received and paid. Its "ยืนยันสั่งซื้อ" is a pending action, a [`Checkout`], that
//! keeps what the summary showed for `session.pendingActionMinutes`; "แก้ไข" starts over, and
//! "ยกเลิก" discards it. Confirming it [places](place) the order in one transaction, or nothing,
//! and shows the updated summary, if anything changed meanwhile (FR-CART-05).
//!
//! What can be checked out is found afresh at every step: the shop's lines that can be ordered
//! (FR-CART-03), at most [`cart::MAX_ITEMS`], none of the buyer's own shop (FR-CART-07), and
//! together at least the shop's minimum order (FR-CART-05). Suspended users can't check out
//! (FR-ADM-04).

mod place;
#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::cart;
use crate::config::{self, Config};
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Location, Turn};
use crate::line::{Action, Message, QuickReply};
use crate::notifications::templates::{self, labels, with_choices};
use crate::persistence::entity::order::{Fulfillment, PaymentMethod};
use crate::persistence::entity::{shop, user};
use crate::thai::{Label, Money};
use crate::users::access::{self, Denied, authorize};
use crate::users::address::{self, Ask, Then};

pub use place::Checkout;

/// The longest note for the shop (FR-CART-04)
pub const NOTE_CHARS: usize = 200;

/// Checking out a shop's lines: the shop, and the choices made so far.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFlow {
    shop_id: i64,
    fulfillment: Option<Fulfillment>,
    payment: Option<PaymentMethod>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// Pickup or delivery
    Fulfillment,
    /// Where to deliver
    Address,
    Payment,
    Note,
}

impl Flow for CheckoutFlow {
    const NAME: &'static str = "checkout";
    type Step = Step;
}

impl Step {
    const ALL: [Self; 4] = [Self::Fulfillment, Self::Address, Self::Payment, Self::Note];
}

/// What checking out a shop's lines would order now: those that can be ordered, in the cart's
/// order.
struct Basket {
    shop: shop::Model,
    lines: Vec<cart::Line>,
}

impl Basket {
    /// What its lines come to, at their prices now.
    fn subtotal(&self) -> Money {
        self.lines.iter().map(cart::Line::total).sum()
    }
}

/// Why a shop's lines can't be checked out.
enum Refusal {
    /// No such shop
    NotFound,
    /// The buyer's own (FR-CART-07)
    OwnShop,
    /// None of its lines can be ordered now (FR-CART-03)
    Nothing(shop::Model),
    /// More than an order holds (FR-CART-07)
    TooMany,
    /// Less than the shop's minimum order (FR-CART-05)
    BelowMinimum(Basket),
}

/// The ways to get an order and to pay for it that a shop takes now (FR-CART-04).
struct Options {
    fulfillments: Vec<Fulfillment>,
    payments: Vec<PaymentMethod>,
}

impl Options {
    /// Whether the shop still takes `fulfillment` and `payment`, those chosen.
    fn take(&self, fulfillment: Option<Fulfillment>, payment: Option<PaymentMethod>) -> bool {
        fulfillment.is_none_or(|chosen| self.fulfillments.contains(&chosen))
            && payment.is_none_or(|chosen| self.payments.contains(&chosen))
    }
}

/// A tap on "สั่งซื้อเลย", "สั่งซื้อร้านนี้" or a summary's "แก้ไข": checks out the shop it names,
/// from the first step, in place of any flow in progress.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    let Some(shop_id) = postback.int(super::SHOP_ID) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    start(turn, config, &turn.actor.user, shop_id, None).await
}

/// Goes on with checking out shop `shop_id` once `user`, as just saved, has an address
/// (FR-ONB-04): from the first step, or, if they changed the address to `deliver` to, from the
/// payment method.
pub async fn resume(
    turn: &Turn<'_>,
    config: &Config,
    user: &user::Model,
    shop_id: i64,
    deliver: bool,
) -> Result<Vec<Message>, DbErr> {
    let chosen = deliver.then_some(Fulfillment::Delivery);
    start(turn, config, user, shop_id, chosen).await
}

/// Checks out shop `shop_id` for `user`, the actor as they are now: from the first step, or
/// past the fulfillment `chosen`, while the shop still offers it. The address is asked for
/// first if they have none.
async fn start(
    turn: &Turn<'_>,
    config: &Config,
    user: &user::Model,
    shop_id: i64,
    chosen: Option<Fulfillment>,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Buy).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let basket = match basket(turn.db, turn, shop_id).await? {
        Ok(basket) => basket,
        Err(refusal) => return Ok(vec![refused(turn, refusal)]),
    };
    let options = match options(turn, config, &basket.shop) {
        Ok(options) => options,
        Err(refused) => return Ok(vec![refused]),
    };
    if !address::has_address(user) {
        let then = Then::Checkout {
            shop_id,
            deliver: false,
        };
        return address::start(turn, config, Ask::All, then).await;
    }
    let mut draft = CheckoutFlow {
        shop_id,
        fulfillment: None,
        payment: None,
    };
    let from = match chosen {
        Some(chosen) if options.fulfillments.contains(&chosen) => {
            draft.fulfillment = Some(chosen);
            Step::Payment
        }
        _ => Step::Fulfillment,
    };
    let (step, ask) = next(turn, user, &basket, &options, &mut draft, from);
    flow::start(turn, Session { step, draft }, ask).await
}

/// The answer to the step the buyer is on (FR-UI-12 step 3), as tapped or typed, or a shared
/// location where the address is confirmed. `None` for anything else, so routing goes on
/// (FR-UI-14). What can be checked out is found afresh first: a suspension, or a shop's lines
/// that can no longer be ordered, end the flow, and choices the shop no longer offers are made
/// again.
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    session: Session<CheckoutFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Session { step, mut draft } = session;
    match (step, input) {
        (_, Input::Text(_)) | (Step::Address, Input::Location(_)) => {}
        _ => return Ok(None),
    }
    let checked = match authorize(turn.db, &turn.actor, access::Action::Buy).await? {
        Err(denied) => Err(turn.denied(denied)),
        Ok(()) => match basket(turn.db, turn, draft.shop_id).await? {
            Err(refusal) => Err(refused(turn, refusal)),
            Ok(basket) => options(turn, config, &basket.shop).map(|options| (basket, options)),
        },
    };
    let (basket, options) = match checked {
        Ok(checked) => checked,
        Err(done) => {
            let done = Answer::<CheckoutFlow>::Done(vec![done]);
            return flow::answered(turn, done).await.map(Some);
        }
    };
    let user = &turn.actor.user;
    if !options.take(draft.fulfillment, draft.payment) {
        return start_over(turn, user, &basket, &options, draft.shop_id)
            .await
            .map(Some);
    }
    let t = turn.templates;
    let text = match input {
        Input::Text(text) => text.trim(),
        _ => "",
    };
    let answer = match step {
        Step::Fulfillment => match fulfillment(text) {
            Some(chosen) if options.fulfillments.contains(&chosen) => {
                draft.fulfillment = Some(chosen);
                let (step, ask) = next(turn, user, &basket, &options, &mut draft, Step::Address);
                Answer::Next(Session { step, draft }, ask)
            }
            _ => Answer::Again(prompt(turn, user, &basket, &options, step)),
        },
        Step::Address => {
            if let &Input::Location(Location {
                latitude,
                longitude,
                address: _,
            }) = input
            {
                let user = share_location(turn, latitude, longitude).await?;
                let (step, ask) = next(turn, &user, &basket, &options, &mut draft, Step::Payment);
                let mut messages = vec![t.location_saved(true)];
                messages.extend(ask);
                Answer::Next(Session { step, draft }, messages)
            } else if text == labels::YES.as_str() {
                let (step, ask) = next(turn, user, &basket, &options, &mut draft, Step::Payment);
                Answer::Next(Session { step, draft }, ask)
            } else if text == labels::CHANGE_ADDRESS.as_str() {
                // In place of this flow, which it comes back to (FR-ONB-04)
                let then = Then::Checkout {
                    shop_id: draft.shop_id,
                    deliver: true,
                };
                return address::start(turn, config, Ask::Address, then)
                    .await
                    .map(Some);
            } else {
                Answer::Again(prompt(turn, user, &basket, &options, step))
            }
        }
        Step::Payment => match payment(text) {
            Some(chosen) if options.payments.contains(&chosen) => {
                draft.payment = Some(chosen);
                let (step, ask) = next(turn, user, &basket, &options, &mut draft, Step::Note);
                Answer::Next(Session { step, draft }, ask)
            }
            _ => Answer::Again(prompt(turn, user, &basket, &options, step)),
        },
        Step::Note => match note(text) {
            Err(Some(max)) => {
                let again = vec![t.text_too_long(max)];
                Answer::Again(with_choices(again, choices(&basket, &options, step)))
            }
            Err(None) => Answer::Again(prompt(turn, user, &basket, &options, step)),
            Ok(note) => {
                let (Some(fulfillment), Some(payment)) = (draft.fulfillment, draft.payment) else {
                    return start_over(turn, user, &basket, &options, draft.shop_id)
                        .await
                        .map(Some);
                };
                let checkout = Checkout::of(&basket, fulfillment, payment, note);
                Answer::Done(vec![checkout.summary(turn, user, &basket).await?])
            }
        },
    };
    flow::answered(turn, answer).await.map(Some)
}

/// The lines of shop `shop_id` the actor can check out now, from their cart in `db`, if they
/// can: buyers still see the shop, it isn't the actor's own (FR-CART-07), some lines can be
/// ordered (FR-CART-03), no more than an order holds (FR-CART-07), coming to its minimum order
/// (FR-CART-05). If not, why.
async fn basket(
    db: &impl ConnectionTrait,
    turn: &Turn<'_>,
    shop_id: i64,
) -> Result<Result<Basket, Refusal>, DbErr> {
    let Some(shop) = shop::Entity::find_by_id(shop_id).one(db).await? else {
        return Ok(Err(Refusal::NotFound));
    };
    if turn.actor.shop.as_ref().map(|own| own.id) == Some(shop.id) {
        return Ok(Err(Refusal::OwnShop));
    }
    let lines: Vec<cart::Line> = cart::lines(db, turn.actor.user.id)
        .await?
        .into_iter()
        .filter(|line| line.shop.id == shop.id && line.state.orderable())
        .collect();
    if lines.is_empty() {
        return Ok(Err(Refusal::Nothing(shop)));
    }
    if lines.len() > cart::MAX_ITEMS {
        return Ok(Err(Refusal::TooMany));
    }
    let basket = Basket { shop, lines };
    Ok(match basket.shop.minimum_order {
        Some(minimum) if basket.subtotal() < minimum => Err(Refusal::BelowMinimum(basket)),
        _ => Ok(basket),
    })
}

/// Why the shop's lines can't be checked out, then where to carry on.
fn refused(turn: &Turn<'_>, refusal: Refusal) -> Message {
    let t = turn.templates;
    let refused = match refusal {
        Refusal::NotFound => return t.denied(Denied::NotFound),
        Refusal::OwnShop => t.own_shop_order(),
        Refusal::Nothing(shop) => t.nothing_to_order(&shop.name),
        Refusal::TooMany => t.too_many_to_order(cart::MAX_ITEMS),
        Refusal::BelowMinimum(basket) => {
            let minimum = basket.shop.minimum_order.unwrap_or(Money::ZERO);
            t.below_minimum_order(&basket.shop.name, minimum, basket.subtotal())
        }
    };
    refused.with_quick_reply(carry_on())
}

/// What `shop` offers to get and pay for an order: the payment methods `payment.methods` allows
/// that it takes, PromptPay only with its PromptPay ID (FR-CART-04). If it takes none of them,
/// why it can't be checked out.
fn options(turn: &Turn<'_>, config: &Config, shop: &shop::Model) -> Result<Options, Message> {
    let mut fulfillments = Vec::new();
    if shop.offers_pickup {
        fulfillments.push(Fulfillment::Pickup);
    }
    if shop.offers_delivery {
        fulfillments.push(Fulfillment::Delivery);
    }
    let payments: Vec<PaymentMethod> = config
        .payment
        .methods
        .iter()
        .map(|&method| match method {
            config::PaymentMethod::Cash => PaymentMethod::Cash,
            config::PaymentMethod::PromptPay => PaymentMethod::PromptPay,
        })
        .filter(|&method| method != PaymentMethod::PromptPay || shop.promptpay_id.is_some())
        .collect();
    let t = turn.templates;
    // A shop offers one way at least (FR-SHOP-01)
    if fulfillments.is_empty() {
        return Err(t.nothing_to_order(&shop.name).with_quick_reply(carry_on()));
    }
    if payments.is_empty() {
        return Err(t.no_payment_method(&shop.name).with_quick_reply(carry_on()));
    }
    Ok(Options {
        fulfillments,
        payments,
    })
}

/// The first step from `from` that has more than one choice, `draft` taking the only one where
/// there is only one, and what asks for it (FR-CART-04). Delivery's address is always
/// confirmed, and the note always asked for, so the summary comes after the note.
fn next(
    turn: &Turn<'_>,
    user: &user::Model,
    basket: &Basket,
    options: &Options,
    draft: &mut CheckoutFlow,
    from: Step,
) -> (Step, Vec<Message>) {
    for step in Step::ALL.into_iter().skip_while(|&step| step != from) {
        let asked = match step {
            Step::Fulfillment => match *options.fulfillments.as_slice() {
                [only] => {
                    draft.fulfillment = Some(only);
                    false
                }
                _ => true,
            },
            Step::Address => draft.fulfillment == Some(Fulfillment::Delivery),
            Step::Payment => match *options.payments.as_slice() {
                [only] => {
                    draft.payment = Some(only);
                    false
                }
                _ => true,
            },
            Step::Note => true,
        };
        if asked {
            return (step, prompt(turn, user, basket, options, step));
        }
    }
    unreachable!("the note is always asked for")
}

/// Starts checking out the basket's shop again from the first step, as it no longer offers what
/// the buyer chose (FR-CART-04).
async fn start_over(
    turn: &Turn<'_>,
    user: &user::Model,
    basket: &Basket,
    options: &Options,
    shop_id: i64,
) -> Result<Vec<Message>, DbErr> {
    let mut draft = CheckoutFlow {
        shop_id,
        fulfillment: None,
        payment: None,
    };
    let (step, ask) = next(turn, user, basket, options, &mut draft, Step::Fulfillment);
    let mut messages = vec![turn.templates.checkout_options_changed()];
    messages.extend(ask);
    flow::answered(turn, Answer::Next(Session { step, draft }, messages)).await
}

/// What asks for `step`, with its choices as Quick Replies; the flow adds the cancel button.
fn prompt(
    turn: &Turn<'_>,
    user: &user::Model,
    basket: &Basket,
    options: &Options,
    step: Step,
) -> Vec<Message> {
    let t = turn.templates;
    let ask = match step {
        Step::Fulfillment => t.ask_pickup_or_delivery(),
        Step::Address => {
            let located = user.latitude.is_some() && user.longitude.is_some();
            t.confirm_delivery_address(&address_of(user), located)
        }
        Step::Payment => t.ask_payment_method(),
        Step::Note => t.ask_order_note(),
    };
    with_choices(vec![ask], choices(basket, options, step))
}

/// The Quick Replies of `step`, sent as typed: as in Appendix A.2, "รับเอง" and "ให้ไปส่ง"
/// with the delivery fee; "ใช่", "เปลี่ยนที่อยู่" and "แชร์ตำแหน่ง"; the payment methods; and
/// "ไม่มี".
fn choices(basket: &Basket, options: &Options, step: Step) -> Vec<Action> {
    let message = |label: &Label| Action::message(label.clone(), label.as_str());
    match step {
        Step::Fulfillment => options
            .fulfillments
            .iter()
            .map(|fulfillment| match fulfillment {
                Fulfillment::Pickup => message(&labels::PICK_UP),
                Fulfillment::Delivery => {
                    let (deliver, fee) = (labels::DELIVER.as_str(), basket.shop.delivery_fee);
                    let label = if fee == Money::ZERO {
                        labels::DELIVER.clone()
                    } else {
                        Label::fit(format!("{deliver} +{fee}"))
                    };
                    Action::message(label, deliver)
                }
            })
            .collect(),
        Step::Address => vec![
            message(&labels::YES),
            message(&labels::CHANGE_ADDRESS),
            Action::Location {
                label: labels::SHARE_LOCATION.clone(),
            },
        ],
        Step::Payment => options
            .payments
            .iter()
            .map(|&payment| match payment {
                PaymentMethod::Cash => message(&labels::CASH_ON_RECEIPT),
                PaymentMethod::PromptPay => message(&labels::PROMPTPAY_TRANSFER),
            })
            .collect(),
        Step::Note => vec![message(&labels::NO_NOTE)],
    }
}

/// The fulfillment `text` chooses, as its Quick Reply sends it.
fn fulfillment(text: &str) -> Option<Fulfillment> {
    if text == labels::PICK_UP.as_str() {
        Some(Fulfillment::Pickup)
    } else if text == labels::DELIVER.as_str() {
        Some(Fulfillment::Delivery)
    } else {
        None
    }
}

/// The payment method `text` chooses, as its Quick Reply sends it.
fn payment(text: &str) -> Option<PaymentMethod> {
    if text == labels::CASH_ON_RECEIPT.as_str() {
        Some(PaymentMethod::Cash)
    } else if text == labels::PROMPTPAY_TRANSFER.as_str() {
        Some(PaymentMethod::PromptPay)
    } else {
        None
    }
}

/// `text` as the note for the shop: none for "ไม่มี". Refused when empty, `Err(None)`, or longer
/// than [`NOTE_CHARS`], `Err(Some(NOTE_CHARS))`.
fn note(text: &str) -> Result<Option<String>, Option<usize>> {
    if text == labels::NO_NOTE.as_str() {
        return Ok(None);
    }
    match text.chars().count() {
        0 => Err(None),
        chars if chars > NOTE_CHARS => Err(Some(NOTE_CHARS)),
        _ => Ok(Some(text.to_owned())),
    }
}

/// `user`'s address, as the summary and the seller see it.
fn address_of(user: &user::Model) -> String {
    let zone = user.zone.as_deref().unwrap_or_default();
    let house_number = user.house_number.as_deref().unwrap_or_default();
    templates::address_line(zone, house_number, user.landmark_note.as_deref())
}

/// Keeps the location the buyer shared to be delivered to as theirs (FR-ONB-04), as an order
/// keeps a copy of the address it is delivered to, but a confirmation keeps no personal data
/// (NFR-SEC-03). Returns them as they now are.
async fn share_location(
    turn: &Turn<'_>,
    latitude: f64,
    longitude: f64,
) -> Result<user::Model, DbErr> {
    let mut user = turn.actor.user.clone();
    user.latitude = Some(latitude);
    user.longitude = Some(longitude);
    user.updated_at = turn.now;
    let row = user::ActiveModel {
        latitude: Set(user.latitude),
        longitude: Set(user.longitude),
        updated_at: Set(turn.now),
        ..Default::default()
    };
    user::Entity::update_many()
        .set(row)
        .filter(user::Column::Id.eq(user.id))
        .exec(turn.db)
        .await?;
    // Not where: it is personal data (NFR-PRIV-05)
    info!("location shared at checkout");
    Ok(user)
}

/// Where to carry on from a checkout that can't go on: "ดูตะกร้า", "เลือกซื้อต่อ" and the menu.
fn carry_on() -> QuickReply {
    QuickReply::new([
        cart::view_cart(),
        cart::continue_shopping(),
        cart::menu_button(),
    ])
}
