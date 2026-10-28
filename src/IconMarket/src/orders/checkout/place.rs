//! Placing an order (FR-CART-05 – FR-CART-08, FR-ORD-01): checkout's summary, confirmed.
//!
//! The summary's "ยืนยันสั่งซื้อ" confirms a [`Checkout`], the pending action that keeps what the
//! summary showed. Confirming it applies it once, in one transaction (FR-UI-13): what it orders
//! is found again and compared with what was shown, the stock is reserved, the order is stored
//! with a copy of each line (DR-03), and its lines leave the cart. If anything changed since the
//! summary, nothing is stored, and the buyer is shown what changed and the summary as it is now.
//!
//! The products are locked first, in ID order, so checkouts of the same products wait for one
//! another rather than deadlock, and stock is taken by a conditional decrement: of checkouts of
//! the last units at the same moment, exactly one places its order and the others are told
//! "สินค้าหมดแล้ว" (FR-CART-06), and stock never goes below 0 (DR-07). The pending action's ID is
//! the order's checkout key, so a repeated tap shows the order rather than placing another.
//!
//! The order's card is the reply, pushed if its reply token has expired (FR-LINE-07), and the
//! seller is told of it with "รับออเดอร์" and "ปฏิเสธ" (FR-ORD-03).

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{Basket, Refusal, address_of, basket, carry_on, refused};
use crate::dispatcher::Turn;
use crate::dispatcher::pending::{self, Applied, Proposal, Stale, Why};
use crate::line::{Action, Message, QuickReply};
use crate::notifications::templates::{CheckoutChange, OrderDetails, OrderLine, labels};
use crate::orders::{actions, cart, contact};
use crate::persistence::entity::order::{self, Fulfillment, PaymentMethod, PaymentStatus, Status};
use crate::persistence::entity::order_event::{self, ActorType};
use crate::persistence::entity::{cart_item, counter, order_item, product, shop, user};
use crate::thai::{self, Money};
use crate::users::access::{self, authorize};
use crate::users::address;

/// A checkout's summary, to confirm (FR-CART-04, FR-UI-13): the shop, how the order is
/// received and paid, the note, and its lines and delivery fee as the summary showed them. The
/// address is not kept: the order copies the buyer's own (NFR-SEC-03).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    shop_id: i64,
    fulfillment: Fulfillment,
    payment: PaymentMethod,
    note: Option<String>,
    /// In product order
    lines: Vec<Shown>,
    /// ฿0 for pickup
    delivery_fee: Money,
}

/// A line as the summary showed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Shown {
    product_id: i64,
    quantity: i32,
    price: Money,
}

impl Checkout {
    /// Ordering `basket`, received by `fulfillment` and paid by `payment`, with `note`.
    pub(super) fn of(
        basket: &Basket,
        fulfillment: Fulfillment,
        payment: PaymentMethod,
        note: Option<String>,
    ) -> Self {
        let mut lines: Vec<Shown> = basket
            .lines
            .iter()
            .map(|line| Shown {
                product_id: line.product.id,
                quantity: line.item.quantity,
                price: line.product.price,
            })
            .collect();
        lines.sort_by_key(|line| line.product_id);
        let delivery_fee = match fulfillment {
            Fulfillment::Pickup => Money::ZERO,
            Fulfillment::Delivery => basket.shop.delivery_fee,
        };
        Self {
            shop_id: basket.shop.id,
            fulfillment,
            payment,
            note,
            lines,
            delivery_fee,
        }
    }

    /// Asks `user` to confirm it for `basket`, which it was made [`of`](Self::of): the summary
    /// (FR-CART-04), with "ยืนยันสั่งซื้อ", "แก้ไข", which starts over, and "ยกเลิก".
    pub(super) async fn summary(
        &self,
        turn: &Turn<'_>,
        user: &user::Model,
        basket: &Basket,
    ) -> Result<Message, DbErr> {
        let confirm = pending::propose(turn, self).await?;
        let buttons = [
            confirm.button(labels::CONFIRM_ORDER.clone()),
            cart::checkout(self.shop_id, &labels::EDIT),
            Action::message(labels::CANCEL.clone(), labels::CANCEL.as_str()),
        ];
        let subtotal = basket.subtotal();
        let order = OrderDetails {
            shop: &basket.shop.name,
            lines: basket
                .lines
                .iter()
                .map(|line| OrderLine {
                    name: &line.product.name,
                    unit: &line.product.unit,
                    price: line.product.price,
                    quantity: line.item.quantity,
                    total: line.total(),
                })
                .collect(),
            subtotal,
            delivery_fee: self.delivery_fee,
            total: [subtotal, self.delivery_fee].into_iter().sum(),
            fulfillment: self.fulfillment,
            address: (self.fulfillment == Fulfillment::Delivery).then(|| address_of(user)),
            payment: self.payment,
            note: self.note.as_deref(),
        };
        Ok(turn.templates.checkout_summary(&order, &buttons))
    }

    /// Whether `basket` and `user` are as the summary showed them: the same lines at the same
    /// prices, the same delivery fee, and the shop still offering what was chosen.
    fn shown(&self, basket: &Basket, user: &user::Model) -> bool {
        let now = Self::of(basket, self.fulfillment, self.payment, self.note.clone());
        now == *self && self.offered(&basket.shop, user)
    }

    /// Whether `shop` still offers how the order is received and paid (FR-CART-04), and `user`
    /// has an address to deliver to if it is delivered.
    fn offered(&self, shop: &shop::Model, user: &user::Model) -> bool {
        let received = match self.fulfillment {
            Fulfillment::Pickup => shop.offers_pickup,
            Fulfillment::Delivery => shop.offers_delivery && address::has_address(user),
        };
        let paid = self.payment != PaymentMethod::PromptPay || shop.promptpay_id.is_some();
        received && paid
    }

    /// What changed of what the summary showed, as `lines`, the shop's in the cart, now show
    /// it: each line that can't be ordered now, or whose price changed, and the delivery fee.
    fn changes<'a>(&self, lines: &'a [cart::Line]) -> Vec<CheckoutChange<'a>> {
        let mut changes = Vec::new();
        for shown in &self.lines {
            let Some(line) = lines
                .iter()
                .find(|line| line.product.id == shown.product_id)
            else {
                continue;
            };
            let (name, unit) = (line.product.name.as_str(), line.product.unit.as_str());
            let change = match line.state {
                cart::State::SoldOut => CheckoutChange::SoldOut { name },
                cart::State::NotEnough(left) => CheckoutChange::NotEnough { name, left, unit },
                cart::State::Unavailable => CheckoutChange::Unavailable { name },
                cart::State::ShopClosed => CheckoutChange::ShopClosed {
                    shop: &line.shop.name,
                },
                cart::State::Orderable if line.product.price != shown.price => {
                    CheckoutChange::Price {
                        name,
                        unit,
                        from: shown.price,
                        to: line.product.price,
                    }
                }
                cart::State::Orderable => continue,
            };
            // A closed shop is said once
            if !changes.contains(&change) {
                changes.push(change);
            }
        }
        if self.fulfillment == Fulfillment::Delivery
            && let Some(shop) = lines.first().map(|line| &line.shop)
            && shop.delivery_fee != self.delivery_fee
        {
            changes.push(CheckoutChange::DeliveryFee {
                from: self.delivery_fee,
                to: shop.delivery_fee,
            });
        }
        changes
    }

    /// Stores the order of `basket`, numbered `number`, keyed by `id`, the pending action
    /// confirmed: `PENDING` and unpaid, with a copy of the buyer's address if it is delivered,
    /// of each line's product as it is now (DR-03), and the first entry of its history
    /// (FR-ORD-02). Returns it and its lines, in the cart's order.
    async fn store(
        &self,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
        basket: &Basket,
        id: Uuid,
        number: String,
    ) -> Result<(order::Model, Vec<order_item::Model>), DbErr> {
        let buyer = &turn.actor.user;
        let delivered = self.fulfillment == Fulfillment::Delivery;
        let copy = |value: &Option<String>| value.clone().filter(|_| delivered);
        let subtotal = basket.subtotal();
        let placed = order::ActiveModel {
            order_number: Set(number),
            checkout_key: Set(id),
            buyer_id: Set(buyer.id),
            shop_id: Set(basket.shop.id),
            status: Set(Status::Pending),
            fulfillment: Set(self.fulfillment),
            delivery_zone: Set(copy(&buyer.zone)),
            delivery_house_number: Set(copy(&buyer.house_number)),
            delivery_landmark_note: Set(copy(&buyer.landmark_note)),
            delivery_latitude: Set(buyer.latitude.filter(|_| delivered)),
            delivery_longitude: Set(buyer.longitude.filter(|_| delivered)),
            note: Set(self.note.clone()),
            subtotal: Set(subtotal),
            delivery_fee: Set(self.delivery_fee),
            total: Set([subtotal, self.delivery_fee].into_iter().sum()),
            payment_method: Set(self.payment),
            payment_status: Set(PaymentStatus::Unpaid),
            created_at: Set(turn.now),
            updated_at: Set(turn.now),
            ..Default::default()
        }
        .insert(txn)
        .await?;
        let lines = basket.lines.iter().map(|line| order_item::ActiveModel {
            order_id: Set(placed.id),
            product_id: Set(line.product.id),
            name: Set(line.product.name.clone()),
            unit: Set(line.product.unit.clone()),
            unit_price: Set(line.product.price),
            quantity: Set(line.item.quantity),
            line_total: Set(line.total()),
            ..Default::default()
        });
        let mut items = order_item::Entity::insert_many(lines)
            .exec_with_returning(txn)
            .await?;
        items.sort_by_key(|item| item.id);
        order_event::ActiveModel {
            order_id: Set(placed.id),
            from_status: Set(None),
            to_status: Set(Status::Pending),
            actor_type: Set(order_event::ActorType::Buyer),
            actor_id: Set(Some(buyer.id)),
            reason: Set(None),
            created_at: Set(turn.now),
            ..Default::default()
        }
        .insert(txn)
        .await?;
        Ok((placed, items))
    }
}

impl Proposal for Checkout {
    const KIND: &'static str = "checkout";

    fn requires(&self) -> access::Action {
        access::Action::Buy
    }

    /// Places the order (FR-CART-05), unless what it orders changed since the summary: the
    /// shop's lines in the cart, which can be ordered and come to its minimum order, are those
    /// shown, at the prices shown, and the shop still offers what was chosen, at the fee shown.
    async fn apply(
        &self,
        id: Uuid,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        // Locked in ID order, so checkouts of the same products wait for one another rather
        // than deadlock, and are then compared as the one before left them
        let ids = self.lines.iter().map(|line| line.product_id);
        product::Entity::find()
            .filter(product::Column::Id.is_in(ids))
            .order_by_asc(product::Column::Id)
            .lock_exclusive()
            .all(txn)
            .await?;
        let basket = match basket(txn, turn, self.shop_id).await? {
            Ok(basket) if self.shown(&basket, &turn.actor.user) => basket,
            _ => return Ok(Applied::Changed),
        };
        // Taken only while as many are left, so stock never goes below 0 (DR-07)
        for line in basket
            .lines
            .iter()
            .filter(|line| line.product.stock.is_some())
        {
            let quantity = line.item.quantity;
            let stock = Expr::col(product::Column::Stock);
            let reserved = product::Entity::update_many()
                .col_expr(product::Column::Stock, stock.sub(quantity))
                .filter(product::Column::Id.eq(line.product.id))
                .filter(product::Column::Stock.gte(quantity))
                .exec(txn)
                .await?;
            if reserved.rows_affected != 1 {
                return Ok(Applied::Changed);
            }
        }
        let number = order_number(txn, turn).await?;
        let (placed, items) = self.store(txn, turn, &basket, id, number).await?;
        let ordered = basket.lines.iter().map(|line| line.product.id);
        cart_item::Entity::delete_many()
            .filter(cart_item::Column::UserId.eq(turn.actor.user.id))
            .filter(cart_item::Column::ProductId.is_in(ordered))
            .exec(txn)
            .await?;
        info!(
            order_id = placed.id,
            order_number = placed.order_number,
            shop_id = placed.shop_id,
            "order placed"
        );
        actions::notify_new_order(turn, &placed, &items, &basket.shop, &turn.actor.user);
        turn.essential();
        let card = order_card(turn, &placed, &items, &basket.shop.name, false);
        Ok(Applied::Done(vec![card]))
    }

    /// The order an earlier tap placed (FR-UI-13); or that nothing was ordered, the summary
    /// having expired or been cancelled; or, what it orders having changed, what changed and
    /// the summary as it is now, to confirm again (AC-12), unless nothing of it can be ordered
    /// now, as when it sold out (AC-03), or the shop no longer offers what was chosen.
    async fn current_state(stale: Stale<Self>, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Buy).await? {
            return Ok(vec![turn.denied(denied)]);
        }
        match (stale.why, stale.proposal) {
            (Why::Confirmed, _) => placed(turn, stale.id).await,
            (Why::Changed, Some(proposal)) => proposal.changed(turn).await,
            (Why::Gone | Why::Changed, proposal) => Ok(vec![gone(turn, proposal.as_ref())]),
        }
    }
}

impl Checkout {
    /// What changed since the summary, and what the buyer can do now.
    async fn changed(&self, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        let (t, user) = (turn.templates, &turn.actor.user);
        let lines: Vec<cart::Line> = cart::lines(turn.db, user.id)
            .await?
            .into_iter()
            .filter(|line| line.shop.id == self.shop_id)
            .collect();
        let changes = self.changes(&lines);
        Ok(match basket(turn.db, turn, self.shop_id).await? {
            Ok(basket) if self.offered(&basket.shop, user) => {
                let again = Self::of(&basket, self.fulfillment, self.payment, self.note.clone());
                let summary = again.summary(turn, user, &basket).await?;
                vec![t.checkout_changed(&changes, true), summary]
            }
            Ok(_) => {
                let again = [
                    cart::checkout(self.shop_id, &labels::ORDER_FROM_SHOP),
                    cart::view_cart(),
                    cart::menu_button(),
                ];
                let changed = t.checkout_options_changed();
                vec![changed.with_quick_reply(QuickReply::new(again))]
            }
            // What changed is why
            Err(Refusal::Nothing(_)) if !changes.is_empty() => {
                vec![
                    t.checkout_changed(&changes, false)
                        .with_quick_reply(carry_on()),
                ]
            }
            Err(refusal) if changes.is_empty() => vec![refused(turn, refusal)],
            Err(refusal) => vec![t.checkout_changed(&changes, false), refused(turn, refusal)],
        })
    }
}

/// The next order number of the day it is at `turn.now` in the business time zone (FR-ORD-01),
/// from the day's counter: `IM-20261022-0007` from `order:20261022`. The counter's row stays
/// locked until the order is stored, so no two orders share a number, and an order not stored
/// takes none.
async fn order_number(txn: &DatabaseTransaction, turn: &Turn<'_>) -> Result<String, DbErr> {
    let day = thai::compact_date(turn.now, turn.templates.time_zone());
    let first = counter::ActiveModel {
        key: Set(format!("order:{day}")),
        value: Set(1),
    };
    let value = Expr::col((counter::Entity, counter::Column::Value));
    let counter = counter::Entity::insert(first)
        .on_conflict(
            OnConflict::column(counter::Column::Key)
                .value(counter::Column::Value, value.add(1))
                .to_owned(),
        )
        .exec_with_returning(txn)
        .await?;
    Ok(format!("IM-{day}-{:04}", counter.value))
}

/// The order that confirming pending action `id` placed (FR-UI-13), as it is now.
async fn placed(turn: &Turn<'_>, id: Uuid) -> Result<Vec<Message>, DbErr> {
    let found = order::Entity::find()
        .filter(order::Column::CheckoutKey.eq(id))
        .filter(order::Column::BuyerId.eq(turn.actor.user.id))
        .one(turn.db)
        .await?;
    let Some(placed) = found else {
        return Ok(vec![gone(turn, None)]);
    };
    let items = order_item::Entity::find()
        .filter(order_item::Column::OrderId.eq(placed.id))
        .order_by_asc(order_item::Column::Id)
        .all(turn.db)
        .await?;
    let shop = shop::Entity::find_by_id(placed.shop_id)
        .one(turn.db)
        .await?;
    let name = shop.map(|shop| shop.name).unwrap_or_default();
    Ok(vec![order_card(turn, &placed, &items, &name, true)])
}

/// Nothing was ordered, the summary having expired or been cancelled: "สั่งซื้อร้านนี้" checks out
/// its shop again, if it is known.
fn gone(turn: &Turn<'_>, proposal: Option<&Checkout>) -> Message {
    let again = proposal.map(|proposal| cart::checkout(proposal.shop_id, &labels::ORDER_FROM_SHOP));
    let actions = again
        .into_iter()
        .chain([cart::view_cart(), cart::menu_button()]);
    turn.templates
        .checkout_gone()
        .with_quick_reply(QuickReply::new(actions))
}

/// The card of order `placed` of shop `shop`, with `items` (FR-CART-08): just placed, or by an
/// earlier tap if `again`. What the buyer can do about it now, "ยกเลิกออเดอร์" while the shop is
/// yet to accept it, then where to carry on.
fn order_card(
    turn: &Turn<'_>,
    placed: &order::Model,
    items: &[order_item::Model],
    shop: &str,
    again: bool,
) -> Message {
    let address = contact::delivery_address(placed);
    let order = OrderDetails {
        shop,
        lines: items
            .iter()
            .map(|item| OrderLine {
                name: &item.name,
                unit: &item.unit,
                price: item.unit_price,
                quantity: item.quantity,
                total: item.line_total,
            })
            .collect(),
        subtotal: placed.subtotal,
        delivery_fee: placed.delivery_fee,
        total: placed.total,
        fulfillment: placed.fulfillment,
        address,
        payment: placed.payment_method,
        note: placed.note.as_deref(),
    };
    let buttons = actions::buttons(placed, ActorType::Buyer);
    let number = &placed.order_number;
    let card = turn
        .templates
        .order_placed(number, placed.status, &order, again, &buttons);
    let next = [cart::continue_shopping(), cart::menu_button()];
    card.with_quick_reply(QuickReply::new(next))
}
