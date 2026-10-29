//! "คำสั่งซื้อของฉัน" and "ออเดอร์ร้าน" (FR-ORD-10), and an order in full from either.
//!
//! "คำสั่งซื้อของฉัน" lists the buyer's orders, open ones first, then the rest, the newest first
//! within each. "ออเดอร์ร้าน" lists the shop's orders by filter: ใหม่, those yet to be accepted,
//! and กำลังดำเนินการ, the active ones, the oldest first, as they are to be handled; ค้างชำระ,
//! those handed over but not paid (FR-PAY-07), and those cancelled once paid, to be refunded
//! (FR-PAY-09); and เสร็จแล้ว/ยกเลิก, those over, the newest first. Both are paged in the
//! database; a page past the last, from an old "ดูเพิ่มเติม", shows the first.
//!
//! Each card shows as much of how to reach the other party as FR-ORD-11 lets its viewer see
//! ([`contact`](super::contact)), and the buttons of what they can do about it now, those of
//! [acting on an order](super::actions), so a seller accepts a new order from the list; its
//! "ดูรายละเอียด" shows the order in full. Suspended users still see their orders (FR-ADM-04),
//! and the owner of a suspended shop its orders, which continue (FR-SHOP-10). A seller who
//! blocked the OA, and so was never told of an order, finds it here (AC-17).

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use sea_orm::sea_query::Order;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, Iterable, QueryFilter, QueryOrder,
    Select,
};

use super::actions::{self, Parties};
use super::{ORDER_ID, VIEW, cart, contact};
use crate::dispatcher::Turn;
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::line::{Action, Message};
use crate::notifications::templates::{ORDERS_PER_PAGE, OrderCard, labels};
use crate::payments::UNPAID;
use crate::persistence::entity::order::{self, PaymentStatus, Status};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{order_item, shop, user};
use crate::persistence::paging::{Page, cut, paged};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// The page of a list its "ดูเพิ่มเติม" asks for, from 0
const PAGE: &str = "page";
/// The filter of "ออเดอร์ร้าน", by its key
const FILTER: &str = "filter";

/// The filters of "ออเดอร์ร้าน" (FR-ORD-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    /// "ใหม่": yet to be accepted
    New,
    /// "กำลังดำเนินการ": accepted, and ready or on its way, but not yet handed over
    Active,
    /// "ค้างชำระ": money still to settle: handed over but not paid (FR-PAY-07), or cancelled
    /// once paid and not yet refunded (FR-PAY-09)
    Unpaid,
    /// "เสร็จแล้ว/ยกเลิก": over
    Done,
}

impl Filter {
    const ALL: [Self; 4] = [Self::New, Self::Active, Self::Unpaid, Self::Done];

    fn key(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Active => "active",
            Self::Unpaid => "unpaid",
            Self::Done => "done",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|filter| filter.key() == key)
    }

    fn label(self) -> &'static Label {
        match self {
            Self::New => &labels::FILTER_NEW,
            Self::Active => &labels::FILTER_ACTIVE,
            Self::Unpaid => &labels::FILTER_UNPAID,
            Self::Done => &labels::FILTER_DONE,
        }
    }

    /// The one that lists `order` as it is now, "ค้างชำระ" while money on it is still to
    /// settle.
    fn of(order: &order::Model) -> Self {
        match order.status {
            Status::Pending => Self::New,
            _ if owed(order) => Self::Unpaid,
            status if status.is_active() => Self::Active,
            _ => Self::Done,
        }
    }

    /// The orders it picks, as [`of`](Self::of) and [`owed`] say.
    fn condition(self) -> Condition {
        let status = order::Column::Status;
        let condition = Condition::all();
        match self {
            Self::New => condition.add(status.eq(Status::Pending)),
            Self::Active => condition.add(status.is_in(Status::iter().filter(|s| s.is_active()))),
            Self::Unpaid => {
                let payment = order::Column::PaymentStatus;
                let unpaid = condition
                    .add(status.eq(Status::Completed))
                    .add(payment.is_in(UNPAID));
                Condition::any()
                    .add(unpaid)
                    .add(payment.eq(PaymentStatus::RefundDue))
            }
            Self::Done => condition.add(status.is_in(Status::iter().filter(|s| s.is_final()))),
        }
    }

    /// Whether it lists the newest first: those over and owed are looked back on, while those
    /// to be handled are, the oldest first.
    fn newest_first(self) -> bool {
        matches!(self, Self::Unpaid | Self::Done)
    }
}

/// Whether `order` is handed over but not paid (FR-PAY-07).
fn unpaid(order: &order::Model) -> bool {
    order.status == Status::Completed && UNPAID.contains(&order.payment_status)
}

/// Whether money on `order` is still to settle: "ค้างชำระ". It is handed over but not paid, or
/// was cancelled once paid and is yet to be refunded (FR-PAY-09).
fn owed(order: &order::Model) -> bool {
    unpaid(order) || order.payment_status == PaymentStatus::RefundDue
}

/// "คำสั่งซื้อของฉัน" (FR-ORD-10): the page its postback asks for.
pub async fn mine(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    mine_page(turn, page(postback)).await
}

/// "ออเดอร์ร้าน" (FR-ORD-10): the filter and page its postback asks for, "ใหม่" by default.
pub async fn shop(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    let filter = postback.name(FILTER).and_then(Filter::from_key);
    shop_page(turn, filter.unwrap_or(Filter::New), page(postback)).await
}

/// The typed keyword "ออเดอร์" (FR-UI-12): for a seller, the new orders of "ออเดอร์ร้าน", the
/// orders they most likely mean; for anyone else, "คำสั่งซื้อของฉัน".
pub async fn typed(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    if runs_shop(turn) {
        shop_page(turn, Filter::New, 0).await
    } else {
        mine_page(turn, 0).await
    }
}

/// "ดูรายละเอียด" (FR-ORD-10): the order of the postback in full, for its buyer or its seller,
/// with what they can do about it now; an admin gets the card they find it by in "ผู้ใช้".
pub async fn view(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let Some(id) = postback.int(ORDER_ID) else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ViewOrder(id)).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let Some(order) = order::Entity::find_by_id(id).one(turn.db).await? else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    let actor = &turn.actor;
    let viewer = if order.buyer_id == actor.user.id {
        ActorType::Buyer
    } else if actor
        .shop
        .as_ref()
        .is_some_and(|shop| shop.id == order.shop_id)
    {
        ActorType::Seller
    } else {
        return Ok(vec![actions::found(turn.db, templates, &order).await?]);
    };
    let parties = Parties::of(turn.db, &order).await?;
    let items = order_item::Entity::find()
        .filter(order_item::Column::OrderId.eq(id))
        .order_by_asc(order_item::Column::Id)
        .all(turn.db)
        .await?;
    let card = card(&order, &items, &parties.shop, &parties.buyer, viewer);
    let back = if viewer == ActorType::Seller {
        filtered(Filter::of(&order)).button(labels::SHOP_ORDERS.clone())
    } else {
        Item::MyOrders.postback().button(labels::MY_ORDERS.clone())
    };
    let back = vec![back, cart::menu_button()];
    Ok(vec![templates.order_details(&card, back)])
}

/// Orders as a list shows them: their cards, and each order with its lines and its shop's
/// name, for the AI assistant to be told about (FR-AI-02).
pub struct Listed {
    pub messages: Vec<Message>,
    pub orders: Vec<ListedOrder>,
}

pub struct ListedOrder {
    pub order: order::Model,
    /// In the order they were put in it
    pub items: Vec<order_item::Model>,
    pub shop: String,
}

/// The AI assistant's `get_my_orders` (FR-AI-02): the first page of "คำสั่งซื้อของฉัน", or of
/// only those orders that are `open`, or only those over.
pub async fn mine_for_assistant(turn: &Turn<'_>, open: Option<bool>) -> Result<Listed, DbErr> {
    mine_listed(turn, 0, open).await
}

/// The AI assistant's `get_shop_orders` (FR-AI-02): the first page of "ออเดอร์ร้าน" with the
/// filter `key` names, "ใหม่" by default.
pub async fn shop_for_assistant(turn: &Turn<'_>, key: Option<&str>) -> Result<Listed, DbErr> {
    let filter = key.and_then(Filter::from_key).unwrap_or(Filter::New);
    shop_listed(turn, filter, 0).await
}

/// The keys of the filters of "ออเดอร์ร้าน", as the AI assistant's `get_shop_orders` takes them.
pub fn filter_keys() -> impl Iterator<Item = &'static str> {
    Filter::ALL.into_iter().map(Filter::key)
}

/// Page `page` of the actor's orders as their buyer, open ones first.
async fn mine_page(turn: &Turn<'_>, page: usize) -> Result<Vec<Message>, DbErr> {
    Ok(mine_listed(turn, page, None).await?.messages)
}

/// Page `page` of the actor's orders as their buyer, open ones first, or only those that are
/// `open`, or only those over. Only the whole list pages on.
async fn mine_listed(turn: &Turn<'_>, page: usize, open: Option<bool>) -> Result<Listed, DbErr> {
    let finals = || Status::iter().filter(|status| status.is_final());
    let select = order::Entity::find().filter(order::Column::BuyerId.eq(turn.actor.user.id));
    let select = match open {
        None => select,
        Some(true) => select.filter(order::Column::Status.is_not_in(finals())),
        Some(false) => select.filter(order::Column::Status.is_in(finals())),
    };
    let select = select
        // Those not over first, as `false` sorts before `true`
        .order_by(order::Column::Status.is_in(finals()), Order::Asc)
        .order_by_desc(order::Column::CreatedAt)
        .order_by_desc(order::Column::Id);
    let found = page_of(turn.db, select, page).await?;
    let parts = Parts::of(turn.db, &found.items).await?;
    let cards = parts.cards(&found.items, ActorType::Buyer)?;
    let more = found
        .next
        .filter(|_| open.is_none())
        .map(|next| more(Item::MyOrders.postback(), next));
    let mut carry_on = vec![cart::continue_shopping()];
    if runs_shop(turn) {
        carry_on.push(filtered(Filter::New).button(labels::SHOP_ORDERS.clone()));
    }
    carry_on.push(cart::menu_button());
    let messages = vec![turn.templates.my_orders(&cards, more, carry_on)];
    Ok(parts.listed(found.items, messages))
}

/// Page `page` of the orders of the actor's shop that `filter` picks. Any shop they own: a
/// suspended one's orders continue (FR-SHOP-10).
async fn shop_page(turn: &Turn<'_>, filter: Filter, page: usize) -> Result<Vec<Message>, DbErr> {
    Ok(shop_listed(turn, filter, page).await?.messages)
}

async fn shop_listed(turn: &Turn<'_>, filter: Filter, page: usize) -> Result<Listed, DbErr> {
    let Some(shop) = &turn.actor.shop else {
        return Ok(Listed {
            messages: vec![turn.denied(Denied::NotAllowed)],
            orders: Vec::new(),
        });
    };
    let select = order::Entity::find()
        .filter(order::Column::ShopId.eq(shop.id))
        .filter(filter.condition());
    let select = if filter.newest_first() {
        select
            .order_by_desc(order::Column::CreatedAt)
            .order_by_desc(order::Column::Id)
    } else {
        select
            .order_by_asc(order::Column::CreatedAt)
            .order_by_asc(order::Column::Id)
    };
    let found = page_of(turn.db, select, page).await?;
    let parts = Parts::of(turn.db, &found.items).await?;
    let cards = parts.cards(&found.items, ActorType::Seller)?;
    let more = found.next.map(|next| more(filtered(filter), next));
    let mut filters: Vec<Action> = Filter::ALL
        .into_iter()
        .map(|filter| filtered(filter).button(filter.label().clone()))
        .collect();
    filters.push(Item::MyOrders.postback().button(labels::MY_ORDERS.clone()));
    filters.push(cart::menu_button());
    let templates = turn.templates;
    let messages = templates.shop_orders(filter.label(), &cards, more, filters);
    Ok(parts.listed(found.items, messages))
}

/// The order numbered `number`, however it is typed, if the actor is its party as `party`
/// says: its buyer, or its seller as the owner of its shop. The AI assistant's proposals take
/// an order by its number (FR-AI-02), as users know it.
pub async fn numbered(
    turn: &Turn<'_>,
    number: &str,
    party: ActorType,
) -> Result<Option<order::Model>, DbErr> {
    let number: String = number
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase();
    let select = order::Entity::find().filter(order::Column::OrderNumber.eq(number));
    let select = match (party, &turn.actor.shop) {
        (ActorType::Buyer, _) => select.filter(order::Column::BuyerId.eq(turn.actor.user.id)),
        (ActorType::Seller, Some(shop)) => select.filter(order::Column::ShopId.eq(shop.id)),
        _ => return Ok(None),
    };
    select.one(turn.db).await
}

/// `order` in full as `viewer` sees it, with only those of its buttons whose action `keep`
/// keeps: the AI assistant's proposal of what to do about it (FR-AI-03), the order's status
/// confirming the tap as for any of its buttons (FR-UI-13). `None` if it keeps none of them, as
/// none of those can be done now.
pub async fn proposal_card(
    turn: &Turn<'_>,
    order: &order::Model,
    viewer: ActorType,
    keep: impl Fn(&str) -> bool,
) -> Result<Option<Message>, DbErr> {
    let (card, kept) = in_full(turn, order, viewer, keep).await?;
    Ok((kept > 0).then_some(card))
}

/// `order` in full as `viewer` sees it, with the buttons of everything they can do about it now.
pub async fn full_card(
    turn: &Turn<'_>,
    order: &order::Model,
    viewer: ActorType,
) -> Result<Message, DbErr> {
    Ok(in_full(turn, order, viewer, |_| true).await?.0)
}

/// `order` in full as `viewer` sees it, with those of its buttons whose action `keep` keeps,
/// and how many it kept.
async fn in_full(
    turn: &Turn<'_>,
    order: &order::Model,
    viewer: ActorType,
    keep: impl Fn(&str) -> bool,
) -> Result<(Message, usize), DbErr> {
    let parties = Parties::of(turn.db, order).await?;
    let items = order_item::Entity::find()
        .filter(order_item::Column::OrderId.eq(order.id))
        .order_by_asc(order_item::Column::Id)
        .all(turn.db)
        .await?;
    let mut card = card(order, &items, &parties.shop, &parties.buyer, viewer);
    card.buttons.retain(|button| match button {
        Action::Postback { data, .. } => {
            Postback::parse(data).is_some_and(|postback| keep(postback.action()))
        }
        _ => false,
    });
    let kept = card.buttons.len();
    let back = if viewer == ActorType::Seller {
        filtered(Filter::of(order)).button(labels::SHOP_ORDERS.clone())
    } else {
        Item::MyOrders.postback().button(labels::MY_ORDERS.clone())
    };
    let back = vec![back, cart::menu_button()];
    Ok((turn.templates.order_details(&card, back), kept))
}

/// Whether the actor runs a shop with orders to handle: one approved, held for review, or
/// suspended, whose orders continue (FR-SHOP-10).
fn runs_shop(turn: &Turn<'_>) -> bool {
    turn.actor.shop.as_ref().is_some_and(|shop| {
        matches!(
            shop.status,
            shop::Status::Approved | shop::Status::Held | shop::Status::Suspended
        )
    })
}

/// "ออเดอร์ร้าน" on "ค้างชำระ", for "ยอดขาย" (FR-SHOP-11) to show the orders it counts unpaid.
pub fn unpaid_list() -> Postback {
    filtered(Filter::Unpaid)
}

/// "ออเดอร์ร้าน" with `filter`.
fn filtered(filter: Filter) -> Postback {
    Item::ShopOrders.postback().with_name(FILTER, filter.key())
}

/// The postback of "ดูเพิ่มเติม" under a page of the list `list`: page `next`.
fn more(list: Postback, next: usize) -> String {
    let next = i64::try_from(next).expect("a few pages");
    list.with_int(PAGE, next).to_string()
}

/// The page `postback` asks for, the first if none.
fn page(postback: &Postback) -> usize {
    postback
        .int(PAGE)
        .and_then(|page| usize::try_from(page).ok())
        .unwrap_or(0)
}

/// Page `page` of the orders `select` finds, [`ORDERS_PER_PAGE`] a page, or the first if there
/// is no such page any more, as orders moved on since its "ดูเพิ่มเติม" was sent.
async fn page_of(
    db: &impl ConnectionTrait,
    select: Select<order::Entity>,
    page: usize,
) -> Result<Page<order::Model>, DbErr> {
    let rows = paged(select.clone(), page, ORDERS_PER_PAGE).all(db).await?;
    let found = cut(rows, page, ORDERS_PER_PAGE);
    if found.items.is_empty() && page > 0 {
        let rows = paged(select, 0, ORDERS_PER_PAGE).all(db).await?;
        return Ok(cut(rows, 0, ORDERS_PER_PAGE));
    }
    Ok(found)
}

/// What the cards of a page of orders show besides the orders: their lines, shops and buyers,
/// each loaded at once for the whole page.
struct Parts {
    /// By order, in the order they were put in it
    items: HashMap<i64, Vec<order_item::Model>>,
    shops: HashMap<i64, shop::Model>,
    buyers: HashMap<i64, user::Model>,
}

impl Parts {
    /// `orders`, each with its lines and its shop's name, after their `messages`.
    fn listed(mut self, orders: Vec<order::Model>, messages: Vec<Message>) -> Listed {
        let orders = orders
            .into_iter()
            .map(|order| ListedOrder {
                items: self.items.remove(&order.id).unwrap_or_default(),
                shop: self
                    .shops
                    .get(&order.shop_id)
                    .map(|shop| shop.name.clone())
                    .unwrap_or_default(),
                order,
            })
            .collect();
        Listed { messages, orders }
    }

    async fn of(db: &impl ConnectionTrait, orders: &[order::Model]) -> Result<Self, DbErr> {
        if orders.is_empty() {
            return Ok(Self {
                items: HashMap::new(),
                shops: HashMap::new(),
                buyers: HashMap::new(),
            });
        }
        let mut items: HashMap<i64, Vec<order_item::Model>> = HashMap::new();
        let all = order_item::Entity::find()
            .filter(order_item::Column::OrderId.is_in(orders.iter().map(|order| order.id)))
            .order_by_asc(order_item::Column::Id)
            .all(db)
            .await?;
        for item in all {
            items.entry(item.order_id).or_default().push(item);
        }
        let shops = shop::Entity::find()
            .filter(shop::Column::Id.is_in(orders.iter().map(|order| order.shop_id)))
            .all(db)
            .await?;
        let buyers = user::Entity::find()
            .filter(user::Column::Id.is_in(orders.iter().map(|order| order.buyer_id)))
            .all(db)
            .await?;
        Ok(Self {
            items,
            shops: shops.into_iter().map(|shop| (shop.id, shop)).collect(),
            buyers: buyers.into_iter().map(|user| (user.id, user)).collect(),
        })
    }

    /// The cards of `orders`, as `viewer` sees them, each with "ดูรายละเอียด". Shops and users
    /// are kept for good (DR-04), so every order has both.
    fn cards<'a>(
        &'a self,
        orders: &'a [order::Model],
        viewer: ActorType,
    ) -> Result<Vec<OrderCard<'a>>, DbErr> {
        orders
            .iter()
            .map(|order| {
                let missing =
                    |what: &str| DbErr::RecordNotFound(format!("{what} of order {}", order.id));
                let shop = self
                    .shops
                    .get(&order.shop_id)
                    .ok_or_else(|| missing("shop"))?;
                let buyer = self
                    .buyers
                    .get(&order.buyer_id)
                    .ok_or_else(|| missing("buyer"))?;
                let items = self.items.get(&order.id).map_or(&[][..], Vec::as_slice);
                let mut card = card(order, items, shop, buyer, viewer);
                let details = Postback::new(VIEW).with_int(ORDER_ID, order.id);
                card.buttons
                    .push(details.button(labels::ORDER_DETAILS.clone()));
                Ok(card)
            })
            .collect()
    }
}

/// The card of `order`, with `items`, at `shop` by `buyer`, as `viewer` sees it: what FR-ORD-11
/// lets them see of how to reach the other party, and the buttons of what they can do now.
fn card<'a>(
    order: &'a order::Model,
    items: &'a [order_item::Model],
    shop: &'a shop::Model,
    buyer: &'a user::Model,
    viewer: ActorType,
) -> OrderCard<'a> {
    OrderCard {
        number: &order.order_number,
        status: order.status,
        placed_at: order.created_at,
        shop: (viewer == ActorType::Buyer).then_some(shop.name.as_str()),
        lines: items.iter().map(actions::order_line).collect(),
        subtotal: order.subtotal,
        delivery_fee: order.delivery_fee,
        total: order.total,
        fulfillment: order.fulfillment,
        payment: order.payment_method,
        payment_status: order.payment_status,
        unpaid: viewer == ActorType::Seller && unpaid(order),
        contact: contact::shown(order, shop, buyer, viewer),
        buttons: actions::buttons(order, viewer),
    }
}
