//! Acting on an order (Table 4.6-B, FR-ORD-02 – FR-ORD-09, FR-ORD-12, FR-PAY-03, FR-PAY-07 –
//! FR-PAY-09): the buttons of its cards and notices, for its seller, its buyer and the admins.
//!
//! The seller is told of a new order with "รับออเดอร์" and "ปฏิเสธ" (FR-ORD-03), as in Appendix
//! A.4, then moves it on: "พร้อมให้รับ" for pickup or "กำลังไปส่ง" for delivery, then
//! "ส่งมอบแล้ว". The buyer cancels an order the shop is yet to accept (FR-ORD-06), asks to cancel
//! one it accepted with "ขอยกเลิก", which the seller grants or not (FR-ORD-07), and confirms
//! receipt with "ได้รับของแล้ว" (FR-ORD-09). An admin finds an order by its number in "ผู้ใช้" and
//! may cancel it while it is open (FR-ORD-12). Rejecting, and cancelling as the seller or an
//! admin, ask why: one of the reasons of FR-ORD-04, or one typed after "อื่นๆ".
//!
//! The buyer of an order paid by PromptPay is told, as it is accepted, what to transfer where
//! (FR-PAY-03), with the PromptPay QR code for its exact amount right under (FR-PAY-04), and
//! "ส่งสลิป" to send the [slip](super::slip) of the transfer (FR-PAY-05). The seller records it
//! paid with "ได้รับเงินแล้ว": a transfer once it arrives, and cash as they hand the order over,
//! when they are asked "ได้รับเงินแล้วหรือยัง?"; one not paid then stays under "ค้างชำระ"
//! (FR-PAY-07). "ยังไม่ได้รับ" on a slip refuses it, and the buyer is told (FR-PAY-05). A paid
//! order cancelled is to be refunded, which they record with "คืนเงินแล้ว" (FR-PAY-09). An
//! admin may record either from the order's card in "ผู้ใช้"; its buyer never can (FR-PAY-08).
//!
//! While an order is active, its buyer and its seller send each other messages with
//! "ส่งข้อความถึงร้าน" and "ส่งข้อความถึงลูกค้า", which the OA [relays](super::relay) (FR-ORD-13).
//!
//! Every move goes through the [lifecycle](super::lifecycle), which checks it again against the
//! order as it is now: a button of a card the order has moved on from, or tapped again, shows
//! its status as it is, with the buttons that now apply (FR-UI-13, FR-ORD-02). Buttons carry
//! only the order's ID (NFR-SEC-03), and offer only the moves the lifecycle allows whoever sees
//! them. Each move tells the other party by push (Table 4.8-A); the buyer's notices follow
//! `notification.orderStatusToBuyer` (FR-ORD-08).

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ConnectionTrait, DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;
use url::Url;

use super::lifecycle::{self, By, Moved};
use super::{
    ACCEPT, ADMIN_CANCEL, CANCEL_ORDER, COMPLETE, DELIVERING, GRANT_CANCEL, KEEP, MARK_PAID,
    MARK_REFUNDED, MESSAGE_BUYER, MESSAGE_SHOP, NOT_PAID, ORDER_ID, RATE, READY, RECEIVED, REJECT,
    REQUEST_CANCEL, SELLER_CANCEL, SEND_SLIP, SLIP_ID, STARS, cart, contact, rating, relay, slip,
};
use crate::admin::audit::{self, Entry, Target};
use crate::admin::reason::{REASON_CHARS, Refused, reason};
use crate::catalog::images::{self, Owner};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message, QuickReply};
use crate::notifications::templates::{
    BUYER_ASKED_TO_CANCEL, FoundOrder, NewOrder, OrderContact, OrderLine, PromptPayDue, Templates,
    labels,
};
use crate::notifications::{Kind, To};
use crate::payments::slip::Answered;
use crate::payments::{self, Recorded};
use crate::persistence::entity::order::{self, PaymentMethod, PaymentStatus, Status};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{order_item, shop, user};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// The reasons offered for rejecting or cancelling an order, sent as if typed (FR-ORD-04),
/// then "อื่นๆ", which asks for one to be typed.
static REASONS: [&Label; 3] = [
    &labels::REASON_SOLD_OUT,
    &labels::REASON_CANT_DELIVER,
    &labels::REASON_SHOP_CLOSED,
];

/// The flow asking why an order is rejected or cancelled (FR-ORD-04).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonFlow {
    order_id: i64,
    act: Act,
}

/// What the reason is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Act {
    /// The seller's "ปฏิเสธ"
    Reject,
    /// The seller's "ยกเลิกออเดอร์" once it is accepted
    SellerCancel,
    /// An admin's "ยกเลิกออเดอร์" (FR-ORD-12)
    AdminCancel,
}

impl Act {
    /// The status the order moves to.
    fn to(self) -> Status {
        match self {
            Self::Reject => Status::Rejected,
            Self::SellerCancel | Self::AdminCancel => Status::Cancelled,
        }
    }

    /// Who moves it.
    fn actor(self) -> ActorType {
        match self {
            Self::Reject | Self::SellerCancel => ActorType::Seller,
            Self::AdminCancel => ActorType::Admin,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// One of the reasons offered, or typed
    Pick,
    /// Typed, "อื่นๆ" having been picked
    Other,
}

impl Flow for ReasonFlow {
    const NAME: &'static str = "order_reason";
    type Step = Step;
}

/// Who an order is between.
pub(super) struct Parties {
    pub shop: shop::Model,
    pub buyer: user::Model,
}

impl Parties {
    /// Those of `order`, which are kept for good: orders refer to them, and shops and users are
    /// only ever soft-deleted or anonymized (DR-04).
    pub async fn of(db: &impl ConnectionTrait, order: &order::Model) -> Result<Self, DbErr> {
        let shop = shop::Entity::find_by_id(order.shop_id)
            .one(db)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("shop of order {}", order.id)))?;
        let buyer = user::Entity::find_by_id(order.buyer_id)
            .one(db)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("buyer of order {}", order.id)))?;
        Ok(Self { shop, buyer })
    }
}

/// A tap on one of an order's buttons (Table 4.6-B), by the action it names.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    use ActorType::{Buyer, Seller};
    let Some(id) = postback.int(ORDER_ID) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    match postback.action() {
        ACCEPT => moves(turn, config, id, Status::Accepted, Seller).await,
        READY => moves(turn, config, id, Status::Ready, Seller).await,
        DELIVERING => moves(turn, config, id, Status::Delivering, Seller).await,
        COMPLETE => moves(turn, config, id, Status::Completed, Seller).await,
        CANCEL_ORDER => moves(turn, config, id, Status::Cancelled, Buyer).await,
        RECEIVED => moves(turn, config, id, Status::Completed, Buyer).await,
        REJECT => ask_reason(turn, id, Act::Reject).await,
        SELLER_CANCEL => ask_reason(turn, id, Act::SellerCancel).await,
        ADMIN_CANCEL => ask_reason(turn, id, Act::AdminCancel).await,
        REQUEST_CANCEL => request_cancel(turn, id).await,
        GRANT_CANCEL => grant_cancel(turn, config, id).await,
        KEEP => keep(turn, id).await,
        MARK_PAID => pays(turn, id, PaymentStatus::Paid).await,
        MARK_REFUNDED => pays(turn, id, PaymentStatus::Refunded).await,
        NOT_PAID => not_paid(turn, id, postback.int(SLIP_ID)).await,
        SEND_SLIP => slip::start(turn, id).await,
        MESSAGE_SHOP => relay::start(turn, id, relay::Side::Buyer).await,
        MESSAGE_BUYER => relay::start(turn, id, relay::Side::Seller).await,
        RATE => rating::tapped(turn, id, postback.int(STARS)).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// The reason, picked or typed (FR-UI-12 step 3): moves the order, unless it moved meanwhile.
/// "อื่นๆ" asks for one to be typed. `None` for anything but text, so routing goes on
/// (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    session: Session<ReasonFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let done = |messages| async move {
        flow::answered(turn, Answer::<ReasonFlow>::Done(messages))
            .await
            .map(Some)
    };
    let Session { step, draft } = session;
    let ReasonFlow { order_id, act } = draft;
    // A suspension, or a lost shop or role, applies to the very next answer (NFR-SEC-10)
    if let Err(denied) = may_act(turn, order_id, act.actor()).await? {
        return done(vec![turn.denied(denied)]).await;
    }
    let templates = turn.templates;
    let text = text.trim();
    let reason = if step == Step::Pick && REASONS.iter().any(|label| label.as_str() == text) {
        text.to_owned()
    } else if text == labels::REASON_OTHER.as_str() {
        let other = Session {
            step: Step::Other,
            draft,
        };
        let ask = vec![templates.ask_other_order_reason(REASON_CHARS)];
        return flow::answered(turn, Answer::Next(other, ask))
            .await
            .map(Some);
    } else {
        match reason(text) {
            Ok(reason) => reason,
            Err(refused) => {
                let again = match (refused, step) {
                    (Refused::TooLong(max), _) => templates.text_too_long(max),
                    (Refused::Empty, Step::Other) => templates.ask_other_order_reason(REASON_CHARS),
                    (Refused::Empty, Step::Pick) => {
                        let Some(order) = order::Entity::find_by_id(order_id).one(turn.db).await?
                        else {
                            return done(vec![templates.denied(Denied::NotFound)]).await;
                        };
                        ask(templates, &order, act)
                    }
                };
                let again = Answer::<ReasonFlow>::Again(vec![again]);
                return flow::answered(turn, again).await.map(Some);
            }
        }
    };
    let messages = apply(turn, config, order_id, act.to(), act.actor(), Some(&reason)).await?;
    done(messages).await
}

/// Tells the seller of `order`, just placed by `buyer` at `shop`, with `items` (FR-ORD-03), as
/// in Appendix A.4: only what FR-ORD-11 shows of an order yet to be accepted, with "รับออเดอร์"
/// and "ปฏิเสธ". Sent unless `notification.newOrderToSeller` is off.
pub(super) fn notify_new_order(
    turn: &Turn<'_>,
    order: &order::Model,
    items: &[order_item::Model],
    shop: &shop::Model,
    buyer: &user::Model,
) {
    let (buyer, zone) = match contact::shown(order, shop, buyer, ActorType::Seller) {
        OrderContact::Buyer { name, zone } => (name, zone),
        _ => (None, None),
    };
    let new = NewOrder {
        lines: items.iter().map(order_line).collect(),
        delivery_fee: order.delivery_fee,
        total: order.total,
        fulfillment: order.fulfillment,
        payment: order.payment_method,
        buyer,
        zone,
    };
    let buttons = buttons(order, ActorType::Seller);
    let notice = turn
        .templates
        .new_order_notice(&order.order_number, &new, buttons);
    turn.notify(Kind::NewOrder, To::User(shop.owner_id), vec![notice]);
}

/// `order` as an admin finds it by its number in "ผู้ใช้" (FR-ADM-04), with "ยกเลิกออเดอร์" while
/// it is open (FR-ORD-12), and "ได้รับเงินแล้ว" or "คืนเงินแล้ว" while it can be recorded so
/// (FR-PAY-08).
pub async fn found(
    db: &impl ConnectionTrait,
    templates: &Templates,
    order: &order::Model,
) -> Result<Message, DbErr> {
    let parties = Parties::of(db, order).await?;
    let rated = rating::of_order(db, order.id).await?;
    let found = FoundOrder {
        number: &order.order_number,
        status: order.status,
        shop: &parties.shop.name,
        buyer: contact::buyer_name(&parties.buyer),
        total: order.total,
        payment: order.payment_method,
        payment_status: order.payment_status,
        placed_at: order.created_at,
        rating: rated.as_ref().map(rating::shown),
    };
    Ok(templates.order_found(&found, buttons(order, ActorType::Admin)))
}

/// What `viewer` can do about `order` as it is now, as buttons: only the moves the lifecycle
/// allows them (Table 4.6-B), and the payments they may record (FR-PAY-08). The seller gets the
/// next step, as in Appendix A.4, "ได้รับเงินแล้ว" or "คืนเงินแล้ว", then "ปฏิเสธ" or
/// "ยกเลิกออเดอร์", or, while the buyer asks to cancel, the answers to that, and
/// "ส่งข้อความถึงลูกค้า" while it is active (FR-ORD-13); the buyer "ยกเลิกออเดอร์" until it is
/// accepted, then "ส่งสลิป" while it awaits their transfer (FR-PAY-05), "ได้รับของแล้ว" once it is
/// ready or on its way, "ขอยกเลิก", "ส่งข้อความถึงร้าน" while it is active, and "ให้คะแนน" once
/// it is completed (FR-ORD-14), which a card of the order drops once it is rated; an admin the
/// payments, and "ยกเลิกออเดอร์" while it is open.
pub(super) fn buttons(order: &order::Model, viewer: ActorType) -> Vec<Action> {
    let can = |to| lifecycle::allowed(order.status, to, viewer, order.fulfillment);
    let acts = |action, label: &Label| button(order, action, label);
    let records = [
        (PaymentStatus::Paid, MARK_PAID, &labels::MARK_PAID),
        (
            PaymentStatus::Refunded,
            MARK_REFUNDED,
            &labels::MARK_REFUNDED,
        ),
    ]
    .into_iter()
    .filter(|&(to, ..)| {
        let (from, method) = (order.payment_status, order.payment_method);
        payments::allowed(from, to, viewer, order.status, method)
    })
    .map(|(_, action, label)| acts(action, label));
    let asked = order.cancel_requested_at.is_some();
    let mut buttons = Vec::new();
    match viewer {
        ActorType::Seller => {
            let steps = [
                (Status::Accepted, ACCEPT, &labels::ACCEPT_ORDER),
                (Status::Ready, READY, &labels::MARK_READY),
                (Status::Delivering, DELIVERING, &labels::MARK_DELIVERING),
                (Status::Completed, COMPLETE, &labels::MARK_COMPLETED),
            ];
            if let Some(&(_, action, label)) = steps.iter().find(|&&(to, ..)| can(to)) {
                buttons.push(acts(action, label));
            }
            buttons.extend(records);
            if can(Status::Rejected) {
                buttons.push(acts(REJECT, &labels::REJECT_ORDER));
            }
            if can(Status::Cancelled) && asked {
                buttons.push(acts(GRANT_CANCEL, &labels::CANCEL_ORDER));
                buttons.push(acts(KEEP, &labels::KEEP_ORDER));
            } else if can(Status::Cancelled) {
                buttons.push(acts(SELLER_CANCEL, &labels::CANCEL_ORDER));
            }
            if order.status.is_active() {
                buttons.push(relay::button(order, relay::Side::Seller));
            }
        }
        ActorType::Buyer => {
            if can(Status::Cancelled) {
                buttons.push(acts(CANCEL_ORDER, &labels::CANCEL_ORDER));
            }
            if payments::slip::awaits_transfer(order) {
                buttons.push(acts(SEND_SLIP, &labels::SEND_SLIP));
            }
            if can(Status::Completed) {
                buttons.push(acts(RECEIVED, &labels::CONFIRM_RECEIPT));
            }
            if requestable(order) && !asked {
                buttons.push(acts(REQUEST_CANCEL, &labels::REQUEST_CANCEL));
            }
            if order.status.is_active() {
                buttons.push(relay::button(order, relay::Side::Buyer));
            }
            if order.status == Status::Completed {
                buttons.push(rating::button(order));
            }
        }
        ActorType::Admin => {
            buttons.extend(records);
            if can(Status::Cancelled) {
                buttons.push(acts(ADMIN_CANCEL, &labels::CANCEL_ORDER));
            }
        }
        ActorType::System => {}
    }
    buttons
}

/// The button `label` that acts on `order` as `action` says.
fn button(order: &order::Model, action: &'static str, label: &Label) -> Action {
    Postback::new(action)
        .with_int(ORDER_ID, order.id)
        .button(label.clone())
}

/// "ได้รับเงินแล้ว" and "ยังไม่ได้รับ": the seller's answers to whether `order`, handed over, was
/// paid (FR-PAY-07).
fn paid_or_not(order: &order::Model) -> Vec<Action> {
    vec![
        button(order, MARK_PAID, &labels::MARK_PAID),
        button(order, NOT_PAID, &labels::NOT_PAID),
    ]
}

/// Whether the buyer may ask to cancel `order` (FR-ORD-07): once accepted, while its seller
/// could still cancel it.
fn requestable(order: &order::Model) -> bool {
    order.status != Status::Pending
        && lifecycle::allowed(
            order.status,
            Status::Cancelled,
            ActorType::Seller,
            order.fulfillment,
        )
}

/// The notices telling buyer and seller `order`, with `parties`, expired unanswered within
/// `minutes` (FR-ORD-05, AC-11).
pub(super) fn expired_notices(
    templates: &Templates,
    order: &order::Model,
    parties: &Parties,
    minutes: u32,
) -> [(Kind, To, Message); 2] {
    let number = &order.order_number;
    let carry_on = vec![cart::continue_shopping()];
    let buyer = templates.order_expired_notice(&parties.shop.name, number, minutes, carry_on);
    let seller = templates.order_expired_seller_notice(number, minutes);
    [
        (Kind::OrderStatus, To::User(order.buyer_id), buyer),
        (Kind::OrderExpired, To::User(parties.shop.owner_id), seller),
    ]
}

/// The order's line `item`, as it was ordered (DR-03).
pub(super) fn order_line(item: &order_item::Model) -> OrderLine<'_> {
    OrderLine {
        name: &item.name,
        unit: &item.unit,
        price: item.unit_price,
        quantity: item.quantity,
        total: item.line_total,
        ready_on: item.ready_on,
    }
}

/// Whether the actor may act on order `id` as `actor` (NFR-SEC-02): as its shop's owner, its
/// buyer or an admin.
async fn may_act(turn: &Turn<'_>, id: i64, actor: ActorType) -> Result<Result<(), Denied>, DbErr> {
    let action = match actor {
        ActorType::Seller => access::Action::HandleOrder(id),
        ActorType::Buyer => access::Action::ActAsBuyer(id),
        ActorType::Admin => access::Action::Administer,
        ActorType::System => return Ok(Err(Denied::NotAllowed)),
    };
    authorize(turn.db, &turn.actor, action).await
}

/// Moves order `id` to `to` as the actor, `actor`, if they may.
async fn moves(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    to: Status,
    actor: ActorType,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = may_act(turn, id, actor).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    apply(turn, config, id, to, actor, None).await
}

/// Moves order `id` to `to` as the actor, `actor`, for `reason`: tells the other party, and
/// answers with what the actor can do next. If it can't move from where it is now, shows it as
/// it is instead (FR-ORD-02).
async fn apply(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    to: Status,
    actor: ActorType,
    reason: Option<&str>,
) -> Result<Vec<Message>, DbErr> {
    let by = By::user(actor, turn.actor.user.id);
    let templates = turn.templates;
    let order = match lifecycle::transition(turn.db, id, to, by, reason, turn.now).await? {
        Moved::Done { order, .. } => order,
        Moved::Refused(order) => return Ok(vec![now_is(templates, &order, actor)]),
        Moved::NotFound => return Ok(vec![templates.denied(Denied::NotFound)]),
    };
    if actor == ActorType::Admin {
        let entry = Entry {
            actor_id: Some(turn.actor.user.id),
            action: audit::Action::OrderCancelled,
            target: Target::Order(order.id),
            reason: reason.map(str::to_owned),
            details: audit::details([("order_number", json!(order.order_number))]),
        };
        audit::record(turn.db, turn.now, [entry]).await?;
    }
    let parties = Parties::of(turn.db, &order).await?;
    let qr = payment_qr(turn, config, &order, &parties.shop).await?;
    for (kind, to, messages) in notices(templates, &order, &parties, actor, qr.as_ref()) {
        turn.notify(kind, to, messages);
    }
    turn.essential();
    Ok(vec![reply(templates, &order, &parties, actor)])
}

/// The notices telling the other party that `actor` moved `order`, with `parties`, to where it
/// is now (Table 4.6-B, Table 4.8-A): the buyer of every move they didn't make (FR-ORD-08), with
/// what to transfer where once one paid by PromptPay is accepted, whatever
/// `notification.orderStatusToBuyer` says, as they can't pay without it (FR-PAY-03), and its `qr`
/// code under, in the same push (FR-PAY-04); the seller of a cancellation they didn't make, and,
/// asked whether they got the money, of the buyer receiving one not paid (FR-PAY-07). Expiry has
/// its own ([`expired_notices`]).
fn notices(
    templates: &Templates,
    order: &order::Model,
    parties: &Parties,
    actor: ActorType,
    qr: Option<&Url>,
) -> Vec<(Kind, To, Vec<Message>)> {
    let (number, shop) = (&order.order_number, &parties.shop.name);
    let (buyer, seller) = (To::User(order.buyer_id), To::User(parties.shop.owner_id));
    let reason = order.cancel_reason.as_deref().unwrap_or_default();
    let refund = order.payment_status == PaymentStatus::RefundDue;
    let next = buttons(order, ActorType::Buyer);
    let carry_on = || vec![cart::continue_shopping()];
    let status = Kind::OrderStatus;
    match (order.status, actor) {
        (Status::Accepted, _) if order.payment_method == PaymentMethod::PromptPay => {
            let due = PromptPayDue {
                id: parties.shop.promptpay_id.as_deref(),
                total: order.total,
            };
            let notice = templates.order_accepted_notice(shop, number, Some(&due), next);
            let qr = qr.map(|url| Message::image(url, url));
            vec![(
                Kind::PaymentDetails,
                buyer,
                [notice].into_iter().chain(qr).collect(),
            )]
        }
        (Status::Accepted, _) => {
            let notice = templates.order_accepted_notice(shop, number, None, next);
            vec![(status, buyer, vec![notice])]
        }
        (Status::Ready | Status::Delivering, _) | (Status::Completed, ActorType::Seller) => {
            let notice = templates.order_progress_notice(shop, number, order.status, next);
            vec![(status, buyer, vec![notice])]
        }
        (Status::Rejected, _) => {
            let notice = templates.order_rejected_notice(shop, number, reason, carry_on());
            vec![(status, buyer, vec![notice])]
        }
        (Status::Cancelled, ActorType::Buyer) => {
            let notice = templates.order_cancelled_by_buyer_notice(number);
            vec![(Kind::OrderCancelled, seller, vec![notice])]
        }
        (Status::Cancelled, ActorType::Seller) => {
            let notice =
                templates.order_cancelled_notice(shop, number, false, reason, refund, carry_on());
            vec![(status, buyer, vec![notice])]
        }
        (Status::Cancelled, ActorType::Admin) => {
            let to_buyer =
                templates.order_cancelled_notice(shop, number, true, reason, refund, carry_on());
            let seller_can = buttons(order, ActorType::Seller);
            let to_seller =
                templates.order_cancelled_by_admin_notice(number, reason, refund, seller_can);
            vec![
                (status, buyer, vec![to_buyer]),
                (Kind::OrderCancelled, seller, vec![to_seller]),
            ]
        }
        // The buyer's own "ได้รับของแล้ว" (FR-ORD-09)
        (Status::Completed, _) if payments::UNPAID.contains(&order.payment_status) => {
            let ask = templates.ask_paid(number, order.total, true, paid_or_not(order));
            vec![(Kind::PaymentToConfirm, seller, vec![ask])]
        }
        (Status::Completed, _) => Vec::new(),
        // Moves no user makes here: placing, and expiry, tell of themselves
        (Status::Pending | Status::Expired, _) | (Status::Cancelled, ActorType::System) => {
            Vec::new()
        }
    }
}

/// The PromptPay QR code for `order`'s exact amount, drawn and stored as it is accepted, if it is
/// paid by PromptPay and `shop` has a PromptPay ID (FR-PAY-04): its link under `app.baseUrl`.
/// Stored in the turn's transaction, so it is there only if the order is accepted.
async fn payment_qr(
    turn: &Turn<'_>,
    config: &Config,
    order: &order::Model,
    shop: &shop::Model,
) -> Result<Option<Url>, DbErr> {
    if order.status != Status::Accepted || order.payment_method != PaymentMethod::PromptPay {
        return Ok(None);
    }
    let payload = shop
        .promptpay_id
        .as_deref()
        .and_then(|id| payments::promptpay_payload(id, Some(order.total)));
    let Some(payload) = payload else {
        return Ok(None);
    };
    let qr = payments::draw_qr(&payload);
    let owner = Owner::PaymentQr(order.id);
    let id = images::store_png(turn.db, owner, qr.png, qr.side, qr.side, turn.now).await?;
    Ok(Some(images::product_image_url(&config.app.base_url, id)))
}

/// What `actor` is told once they moved `order`, with `parties`, and what they can do next:
/// how to reach the buyer once the seller accepted it, as in Appendix A.4 (FR-ORD-11), and
/// whether they got the money once they handed it over unpaid (FR-PAY-07).
fn reply(
    templates: &Templates,
    order: &order::Model,
    parties: &Parties,
    actor: ActorType,
) -> Message {
    let (number, next) = (&order.order_number, buttons(order, actor));
    let shown = contact::shown(order, &parties.shop, &parties.buyer, actor);
    let unpaid = payments::UNPAID.contains(&order.payment_status);
    let reply = match (order.status, shown) {
        (Status::Accepted, OrderContact::BuyerFull { contact, .. }) => {
            templates.order_accepted(number, &contact, next)
        }
        (Status::Completed, _) if actor == ActorType::Seller && unpaid => {
            templates.ask_paid(number, order.total, false, paid_or_not(order))
        }
        (status, _) => {
            let refund = order.payment_status == PaymentStatus::RefundDue;
            templates.order_moved(number, status, actor, refund, next)
        }
    };
    if actor == ActorType::Buyer && order.status.is_final() {
        let carry_on = [cart::continue_shopping(), cart::menu_button()];
        return reply.with_quick_reply(QuickReply::new(carry_on));
    }
    reply
}

/// `order` as it is now, for `viewer`, as what they asked of it can't be done: its status, and
/// the buttons that now apply (FR-UI-13, FR-ORD-02).
pub(super) fn now_is(templates: &Templates, order: &order::Model, viewer: ActorType) -> Message {
    let buttons = buttons(order, viewer);
    templates.order_now(&order.order_number, order.status, None, buttons)
}

/// `order` as it is now, with how far its payment is, for `viewer`, as what they asked to
/// record of it can't be (FR-PAY-08): recorded meanwhile, or the same button tapped again.
pub(super) fn paid_now_is(
    templates: &Templates,
    order: &order::Model,
    viewer: ActorType,
) -> Message {
    let (number, payment) = (&order.order_number, Some(order.payment_status));
    templates.order_now(number, order.status, payment, buttons(order, viewer))
}

/// Asks why order `id` is moved as `act` says, if the actor may and the lifecycle allows it
/// from where it is now; otherwise shows it as it is.
async fn ask_reason(turn: &Turn<'_>, id: i64, act: Act) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = may_act(turn, id, act.actor()).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let templates = turn.templates;
    let Some(order) = order::Entity::find_by_id(id).one(turn.db).await? else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    if !lifecycle::allowed(order.status, act.to(), act.actor(), order.fulfillment) {
        return Ok(vec![now_is(templates, &order, act.actor())]);
    }
    let session = Session {
        step: Step::Pick,
        draft: ReasonFlow { order_id: id, act },
    };
    flow::start(turn, session, vec![ask(templates, &order, act)]).await
}

/// What asks why `order` is moved as `act` says, offering FR-ORD-04's reasons and "อื่นๆ".
fn ask(templates: &Templates, order: &order::Model, act: Act) -> Message {
    let reasons = REASONS
        .into_iter()
        .chain([&labels::REASON_OTHER])
        .map(|label| Action::message(label.clone(), label.as_str()))
        .collect();
    let by_admin = act == Act::AdminCancel;
    templates.ask_order_reason(&order.order_number, act.to(), by_admin, reasons)
}

/// The buyer's "ขอยกเลิก" on order `id` (FR-ORD-07): the seller is asked to decide, once. Shows
/// the order as it is if it can no longer be asked.
async fn request_cancel(turn: &Turn<'_>, id: i64) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = may_act(turn, id, ActorType::Buyer).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let templates = turn.templates;
    let Some(order) = lifecycle::lock(turn.db, id).await? else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    if !requestable(&order) {
        return Ok(vec![now_is(templates, &order, ActorType::Buyer)]);
    }
    if order.cancel_requested_at.is_some() {
        return Ok(vec![
            templates.cancellation_requested(&order.order_number, true),
        ]);
    }
    let order = order::ActiveModel {
        id: Unchanged(id),
        cancel_requested_at: Set(Some(turn.now)),
        updated_at: Set(turn.now),
        ..Default::default()
    }
    .update(turn.db)
    .await?;
    let parties = Parties::of(turn.db, &order).await?;
    let number = &order.order_number;
    let buttons = buttons(&order, ActorType::Seller);
    let notice = templates.cancellation_requested_notice(number, buttons);
    turn.notify(
        Kind::CancellationRequested,
        To::User(parties.shop.owner_id),
        vec![notice],
    );
    info!(order_id = id, "cancellation requested");
    turn.essential();
    Ok(vec![templates.cancellation_requested(number, false)])
}

/// The seller's "ยกเลิกออเดอร์" on the buyer's request to cancel order `id` (FR-ORD-07): cancels
/// it, the request being the reason.
async fn grant_cancel(turn: &Turn<'_>, config: &Config, id: i64) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = may_act(turn, id, ActorType::Seller).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let Some(order) = lifecycle::lock(turn.db, id).await? else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    if order.cancel_requested_at.is_none() {
        return Ok(vec![now_is(turn.templates, &order, ActorType::Seller)]);
    }
    let reason = Some(BUYER_ASKED_TO_CANCEL);
    apply(
        turn,
        config,
        id,
        Status::Cancelled,
        ActorType::Seller,
        reason,
    )
    .await
}

/// The seller's "ไม่ยกเลิก" on the buyer's request to cancel order `id` (FR-ORD-07): goes on with
/// it, and tells the buyer, who may ask again.
async fn keep(turn: &Turn<'_>, id: i64) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = may_act(turn, id, ActorType::Seller).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let templates = turn.templates;
    let Some(order) = lifecycle::lock(turn.db, id).await? else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    if order.cancel_requested_at.is_none() || order.status.is_final() {
        return Ok(vec![now_is(templates, &order, ActorType::Seller)]);
    }
    let order = order::ActiveModel {
        id: Unchanged(id),
        cancel_requested_at: Set(None),
        updated_at: Set(turn.now),
        ..Default::default()
    }
    .update(turn.db)
    .await?;
    let parties = Parties::of(turn.db, &order).await?;
    let (number, shop) = (&order.order_number, &parties.shop.name);
    let kept = templates.cancellation_kept_notice(shop, number, buttons(&order, ActorType::Buyer));
    turn.notify(Kind::OrderStatus, To::User(order.buyer_id), vec![kept]);
    info!(order_id = id, "cancellation request declined");
    turn.essential();
    let next = buttons(&order, ActorType::Seller);
    Ok(vec![templates.cancellation_kept(number, next)])
}

/// The seller's, or an admin's, "ได้รับเงินแล้ว" or "คืนเงินแล้ว" on order `id`: records it as
/// `to` if it may be from where it is now (FR-PAY-08), tells the buyer, and audits an admin's.
/// Shows the order as it is otherwise.
async fn pays(turn: &Turn<'_>, id: i64, to: PaymentStatus) -> Result<Vec<Message>, DbErr> {
    let actor = match payer(turn, id).await? {
        Ok(actor) => actor,
        Err(denied) => return Ok(vec![turn.denied(denied)]),
    };
    let by = By::user(actor, turn.actor.user.id);
    let templates = turn.templates;
    let order = match payments::record(turn.db, id, to, by, turn.now).await? {
        Recorded::Done { order, .. } => order,
        Recorded::Refused(order) => return Ok(vec![paid_now_is(templates, &order, actor)]),
        Recorded::NotFound => return Ok(vec![templates.denied(Denied::NotFound)]),
    };
    let by_admin = actor == ActorType::Admin;
    if by_admin {
        let action = match to {
            PaymentStatus::Refunded => audit::Action::RefundConfirmed,
            _ => audit::Action::PaymentConfirmed,
        };
        let entry = Entry {
            actor_id: Some(turn.actor.user.id),
            action,
            target: Target::Order(order.id),
            reason: None,
            details: audit::details([("order_number", json!(order.order_number))]),
        };
        audit::record(turn.db, turn.now, [entry]).await?;
    }
    let parties = Parties::of(turn.db, &order).await?;
    let (number, shop) = (&order.order_number, &parties.shop.name);
    let next = buttons(&order, ActorType::Buyer);
    let told = templates.payment_recorded_notice(shop, number, to, by_admin, next);
    turn.notify(Kind::OrderStatus, To::User(order.buyer_id), vec![told]);
    turn.essential();
    let next = buttons(&order, actor);
    Ok(vec![templates.payment_recorded(number, to, next)])
}

/// As whom the actor records the payment of order `id` (FR-PAY-08): as its seller if its shop
/// is theirs, else as an admin if they are one.
async fn payer(turn: &Turn<'_>, id: i64) -> Result<Result<ActorType, Denied>, DbErr> {
    let as_seller = may_act(turn, id, ActorType::Seller).await?;
    if as_seller.is_err() && turn.actor.user.is_admin {
        let as_admin = may_act(turn, id, ActorType::Admin).await?;
        return Ok(as_admin.map(|()| ActorType::Admin));
    }
    Ok(as_seller.map(|()| ActorType::Seller))
}

/// The seller's "ยังไม่ได้รับ" on order `id`: on the notice of its slip `slip` (FR-PAY-05), or
/// when asked whether it was paid as they handed it over (FR-PAY-07). A slip awaiting their
/// answer is refused, and the buyer is told; otherwise nothing changes, and an order handed
/// over stays under "ค้างชำระ" until they record it paid. Shows the order as it is if it was
/// answered or recorded meanwhile, or another slip came since.
async fn not_paid(turn: &Turn<'_>, id: i64, slip: Option<i64>) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = may_act(turn, id, ActorType::Seller).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let templates = turn.templates;
    let Some(order) = order::Entity::find_by_id(id).one(turn.db).await? else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    if order.payment_status == PaymentStatus::AwaitingConfirmation {
        return slip_not_received(turn, id, slip).await;
    }
    let unpaid = payments::UNPAID.contains(&order.payment_status);
    if slip.is_some() || order.status != Status::Completed || !unpaid {
        return Ok(vec![paid_now_is(templates, &order, ActorType::Seller)]);
    }
    let next = buttons(&order, ActorType::Seller);
    Ok(vec![templates.left_unpaid(&order.order_number, next)])
}

/// The seller didn't get the transfer of order `id`, whose slip `slip`, or the one awaiting
/// their answer, is refused (FR-PAY-05): the order awaits a transfer again, and the buyer is
/// told, with "ส่งสลิป" while they can send another.
async fn slip_not_received(
    turn: &Turn<'_>,
    id: i64,
    slip: Option<i64>,
) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let order = match payments::slip::not_received(turn.db, id, slip, turn.now).await? {
        Answered::Done(order) => order,
        Answered::Refused(order) => {
            return Ok(vec![paid_now_is(templates, &order, ActorType::Seller)]);
        }
        Answered::NotFound => return Ok(vec![templates.denied(Denied::NotFound)]),
    };
    let parties = Parties::of(turn.db, &order).await?;
    let (number, shop) = (&order.order_number, &parties.shop.name);
    let next = buttons(&order, ActorType::Buyer);
    let told = templates.slip_not_received_notice(shop, number, order.total, next);
    turn.notify(Kind::SlipNotReceived, To::User(order.buyer_id), vec![told]);
    turn.essential();
    let next = buttons(&order, ActorType::Seller);
    Ok(vec![templates.slip_not_received(number, next)])
}
