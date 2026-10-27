//! The cart (FR-CART-01 – FR-CART-03): what a buyer means to order, kept in `cart_items` until
//! it is checked out or removed (FR-CART-02).
//!
//! "ใส่ตะกร้า" on a product's card asks how many, with Quick Replies 1–5 and "ระบุเอง", in a
//! [flow](QuantityFlow) of one step. The Quick Replies send their number as if typed, so a
//! quantity is never carried by a postback (NFR-SEC-03). A line holds 1–99 of its product, and
//! no more than is left: a quantity over that is refused, saying how many are left (FR-CART-01).
//! Adding a product already in the cart adds to its line, so the limit counts what the line
//! holds. Nothing is reserved: checkout takes the stock (#32). A cart holds at most
//! [`MAX_ITEMS`] products, as many as an order may (FR-CART-07), which also keeps the whole cart
//! within what LINE's carousels hold (FR-LINE-11). A seller can't put their own shop's products
//! in it (FR-CART-07).
//!
//! Once added, the reply offers "สั่งซื้อเลย", which checks out that shop's lines (#32), "ดูตะกร้า"
//! and "เลือกซื้อต่อ". "ตะกร้า" shows the cart a card per shop, the shop added to last first: each
//! line's name, quantity, unit price and total, with "แก้จำนวน" and "ลบออก", then the shop's
//! subtotal and "สั่งซื้อร้านนี้". A line that can't be ordered now ([`State`]) is flagged, and
//! left out of the subtotal and of the checkout (FR-CART-03).
//!
//! Every button carries only IDs and page numbers, and each tap finds what it names afresh: a
//! product buyers no longer see is not found, and one that can no longer be ordered is refused
//! (FR-UI-13). Suspended users can't use the cart (FR-ADM-04).

#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter, QuerySelect,
    RelationTrait,
};
use serde::{Deserialize, Serialize};

use crate::catalog::browse::listing;
use crate::catalog::products::Availability;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message, QuickReply};
use crate::notifications::templates::{
    CART_SHOPS_PER_PAGE, CartLine, CartShop, labels, with_choices,
};
use crate::orders;
use crate::persistence::entity::{cart_item, product, shop, user};
use crate::thai::{self, Label, Money, NumberError};
use crate::users::access::{self, Denied, authorize};

/// What the actions of the cart's buttons start with, "ใส่ตะกร้า" ([`orders::ADD_TO_CART`])
/// included, but the cart's own, which is [`Item::Cart`]. Each names its product in
/// [`orders::PRODUCT_ID`].
pub const PREFIX: &str = "cart.";
/// "แก้จำนวน" on a line: asks for its new quantity
const QUANTITY: &str = "cart.quantity";
/// "ลบออก" on a line
const REMOVE: &str = "cart.remove";
/// The page of the cart "ดูเพิ่มเติม" shows, on the postback of [`Item::Cart`]
const PAGE: &str = "page";

/// The most of a product a line may hold (FR-CART-01)
pub const MAX_QUANTITY: i32 = 99;
/// The most products a cart may hold: as many as an order may (FR-CART-07)
pub const MAX_ITEMS: usize = 20;
/// The quantities offered as Quick Replies, from 1 (FR-CART-01)
const QUICK_QUANTITIES: i32 = 5;

/// A line of a buyer's cart as it is now: what it holds, of which product of which shop, and
/// whether it can be ordered.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub item: cart_item::Model,
    pub product: product::Model,
    pub shop: shop::Model,
    pub state: State,
}

impl Line {
    /// Its total: the product's price now, times its quantity.
    pub fn total(&self) -> Money {
        self.product.price.times(self.item.quantity)
    }
}

/// Whether a line can be checked out now (FR-CART-03).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// As many as it holds are left, or the product is unlimited
    Orderable,
    /// *หมด* (FR-PROD-06)
    SoldOut,
    /// *ร้านปิด*: its shop takes no orders now (FR-SHOP-05)
    ShopClosed,
    /// Not for sale: hidden, held for review, removed or deleted (FR-PROD-07)
    Unavailable,
    /// Fewer are left than it holds: this many
    NotEnough(i32),
}

impl State {
    /// That of a line holding `quantity` of `product` of `shop`.
    pub fn of(product: &product::Model, shop: &shop::Model, quantity: i32) -> Self {
        if product.status != product::Status::Active {
            return Self::Unavailable;
        }
        match Availability::of(product, shop) {
            Availability::ShopClosed => Self::ShopClosed,
            Availability::SoldOut => Self::SoldOut,
            Availability::Left(left) if left < quantity => Self::NotEnough(left),
            Availability::Left(_) | Availability::Orderable => Self::Orderable,
        }
    }

    /// Whether the line can be checked out.
    pub fn orderable(self) -> bool {
        self == Self::Orderable
    }

    /// Whether the line's quantity can be changed: while its product can be ordered at all.
    fn changeable(self) -> bool {
        matches!(self, Self::Orderable | Self::NotEnough(_))
    }
}

/// User `user_id`'s cart, its lines grouped by shop: the shop added to last first, then each
/// shop's lines in the order they were added.
pub async fn lines(db: &impl ConnectionTrait, user_id: i64) -> Result<Vec<Line>, DbErr> {
    let items = items(db, user_id).await?;
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = items.iter().map(|item| item.product_id).collect();
    let mut products: HashMap<i64, (product::Model, shop::Model)> = product::Entity::find()
        .join(JoinType::InnerJoin, product::Relation::Shop.def())
        .filter(product::Column::Id.is_in(ids))
        .select_two_required(shop::Entity)
        .all(db)
        .await?
        .into_iter()
        .map(|(product, shop)| (product.id, (product, shop)))
        .collect();
    // A suspended user's shop takes no orders, as a suspended shop takes none (FR-ADM-04)
    let suspended: HashSet<i64> = user::Entity::find()
        .select_only()
        .column(user::Column::Id)
        .filter(user::Column::Id.is_in(products.values().map(|(_, shop)| shop.owner_id)))
        .filter(user::Column::Status.ne(user::Status::Active))
        .into_tuple::<i64>()
        .all(db)
        .await?
        .into_iter()
        .collect();
    let mut lines: Vec<Line> = items
        .into_iter()
        .filter_map(|item| {
            let (product, shop) = products.remove(&item.product_id)?;
            let state = match State::of(&product, &shop, item.quantity) {
                State::Unavailable => State::Unavailable,
                _ if suspended.contains(&shop.owner_id) => State::ShopClosed,
                state => state,
            };
            Some(Line {
                item,
                product,
                shop,
                state,
            })
        })
        .collect();
    let mut latest: HashMap<i64, DateTimeUtc> = HashMap::new();
    for line in &lines {
        let added = latest.entry(line.shop.id).or_insert(line.item.added_at);
        *added = (*added).max(line.item.added_at);
    }
    lines.sort_by_key(|line| {
        let item = &line.item;
        (
            Reverse(latest[&line.shop.id]),
            line.shop.id,
            item.added_at,
            item.product_id,
        )
    });
    Ok(lines)
}

/// The lines of user `user_id`'s cart as kept, whatever their products now.
async fn items(db: &impl ConnectionTrait, user_id: i64) -> Result<Vec<cart_item::Model>, DbErr> {
    cart_item::Entity::find()
        .filter(cart_item::Column::UserId.eq(user_id))
        .all(db)
        .await
}

/// Asking how many of a product to put in the cart, or to change its line to. Its draft names
/// the product.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuantityFlow {
    product_id: i64,
    /// Whether the answer replaces what the line holds, from "แก้จำนวน", rather than adds to it
    change: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Quantity,
}

impl Flow for QuantityFlow {
    const NAME: &'static str = "cart_quantity";
    type Step = Step;
}

/// A tap on "ตะกร้า", "ใส่ตะกร้า", or a button of the cart.
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Buy).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let action = postback.action();
    if action == Item::Cart.action() {
        let page = postback
            .int(PAGE)
            .and_then(|page| usize::try_from(page).ok())
            .unwrap_or(0);
        return Ok(vec![cart(turn, page).await?]);
    }
    let Some(id) = postback.int(orders::PRODUCT_ID) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    match action {
        orders::ADD_TO_CART => ask(turn, id, false).await,
        QUANTITY => ask(turn, id, true).await,
        REMOVE => remove(turn, id).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// "ตะกร้า", typed (FR-UI-12 step 4): the cart's first page.
pub async fn show(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Buy).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    Ok(vec![cart(turn, 0).await?])
}

/// What a line of a product can take now, as the user asks to put it in the cart.
struct Room {
    product: product::Model,
    /// What the line holds now; 0 if the product isn't in the cart
    in_cart: i32,
    /// Whether the answer replaces what the line holds, rather than adds to it
    change: bool,
    /// The most the line may hold: [`MAX_QUANTITY`], or fewer if fewer are left
    limit: i32,
}

impl Room {
    /// The most the answer may be.
    fn most(&self) -> i32 {
        if self.change {
            self.limit
        } else {
            (self.limit - self.in_cart).max(0)
        }
    }

    /// What the line holds once `quantity` is put in it.
    fn after(&self, quantity: i32) -> i32 {
        if self.change {
            quantity
        } else {
            self.in_cart + quantity
        }
    }
}

/// What product `id`'s line in the user's cart can take now, if it can go in it: buyers still
/// see it (FR-SRCH-02), it isn't of the user's own shop (FR-CART-07), it can be ordered
/// (FR-PROD-06, FR-SHOP-05), and the cart has room for one more product (FR-CART-07). If not,
/// why. `change` is changing its line, which becomes adding to it once the line is gone.
async fn room(turn: &Turn<'_>, id: i64, change: bool) -> Result<Result<Room, Message>, DbErr> {
    let t = turn.templates;
    let Some((product, shop)) = listing::product(turn.db, id).await? else {
        return Ok(Err(t.denied(Denied::NotFound)));
    };
    if turn.actor.shop.as_ref().map(|own| own.id) == Some(shop.id) {
        return Ok(Err(t.own_product().with_quick_reply(next())));
    }
    let availability = Availability::of(&product, &shop);
    if !availability.orderable() {
        let refused = t.not_orderable(&product.name, &shop.name, availability);
        return Ok(Err(refused.with_quick_reply(next())));
    }
    let items = items(turn.db, turn.actor.user.id).await?;
    let in_cart = items
        .iter()
        .find(|item| item.product_id == id)
        .map_or(0, |item| item.quantity);
    if in_cart == 0 && items.len() >= MAX_ITEMS {
        let actions = [view_cart(), menu_button()];
        return Ok(Err(t
            .cart_full(MAX_ITEMS)
            .with_quick_reply(QuickReply::new(actions))));
    }
    let limit = product
        .stock
        .map_or(MAX_QUANTITY, |left| left.min(MAX_QUANTITY));
    Ok(Ok(Room {
        product,
        in_cart,
        change: change && in_cart > 0,
        limit,
    }))
}

/// "ใส่ตะกร้า", or "แก้จำนวน" if `change`, on product `id`: how many, in place of any flow in
/// progress (FR-CART-01). Nothing is asked when the line is full already.
async fn ask(turn: &Turn<'_>, id: i64, change: bool) -> Result<Vec<Message>, DbErr> {
    let room = match room(turn, id, change).await? {
        Ok(room) => room,
        Err(refused) => return Ok(vec![refused]),
    };
    if room.most() == 0 {
        return Ok(vec![too_many(turn, &room)]);
    }
    let session = Session {
        step: Step::Quantity,
        draft: QuantityFlow {
            product_id: id,
            change: room.change,
        },
    };
    flow::start(turn, session, question(turn, &room)).await
}

/// The question how many, with the quantities that fit as Quick Replies.
fn question(turn: &Turn<'_>, room: &Room) -> Vec<Message> {
    let product = &room.product;
    let ask =
        turn.templates
            .ask_cart_quantity(&product.name, &product.unit, room.in_cart, room.change);
    with_choices(vec![ask], quantities(room.most()))
}

/// The quantity, tapped or typed (FR-UI-12 step 3): the line holds it, or that much more, if
/// the product can still go in the cart and as many are left. `None` for anything but text, so
/// routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    session: Session<QuantityFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let QuantityFlow { product_id, change } = session.draft;
    // A suspension ends it (NFR-SEC-10), and so does a product that can no longer go in the cart,
    // or a line that can take no more, as fewer are left than when asked
    let room = match authorize(turn.db, &turn.actor, access::Action::Buy).await? {
        Err(denied) => Err(turn.denied(denied)),
        Ok(()) => room(turn, product_id, change).await?,
    };
    let done = match room {
        Ok(room) if room.most() == 0 => Err(too_many(turn, &room)),
        Ok(room) => Ok(room),
        Err(refused) => Err(refused),
    };
    let room = match done {
        Ok(room) => room,
        Err(done) => {
            let done = Answer::<QuantityFlow>::Done(vec![done]);
            return flow::answered(turn, done).await.map(Some);
        }
    };
    let text = text.trim();
    let answer = if text == labels::TYPE_QUANTITY.as_str() {
        let unit = &room.product.unit;
        Answer::<QuantityFlow>::Again(vec![turn.templates.ask_typed_quantity(room.most(), unit)])
    } else {
        match quantity(text, &room.product.unit) {
            Ok(quantity) if quantity > room.most() => {
                let again = turn.templates.quantity_too_many(
                    &room.product.name,
                    &room.product.unit,
                    room.product.stock,
                    MAX_QUANTITY,
                    if room.change { 0 } else { room.in_cart },
                );
                Answer::Again(with_choices(vec![again], quantities(room.most())))
            }
            Ok(quantity) => Answer::Done(put(turn, &room, quantity).await?),
            Err(Refused::Empty) => Answer::Again(question(turn, &room)),
            Err(Refused::Zero) => {
                let again = turn.templates.quantity_too_small(&room.product.unit);
                Answer::Again(with_choices(vec![again], quantities(room.most())))
            }
            Err(Refused::NotANumber(e)) => {
                let again = Message::text(turn.templates.number_problem(e));
                Answer::Again(with_choices(vec![again], quantities(room.most())))
            }
        }
    };
    flow::answered(turn, answer).await.map(Some)
}

/// Why typed text is not a quantity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    Empty,
    Zero,
    NotANumber(NumberError),
}

/// `text` as a quantity of a product sold by `unit`: a whole number of at least 1, in Arabic or
/// Thai digits, which may be followed by the unit (`2 แผง`). A number too large to read is
/// `i32::MAX`, more than any line takes.
fn quantity(text: &str, unit: &str) -> Result<i32, Refused> {
    let count = text.strip_suffix(unit).unwrap_or(text).trim_end();
    if count.is_empty() {
        return Err(Refused::Empty);
    }
    match thai::parse_whole(count) {
        Ok(0) => Err(Refused::Zero),
        Ok(count) => Ok(i32::try_from(count).unwrap_or(i32::MAX)),
        Err(NumberError::TooLarge) => Ok(i32::MAX),
        Err(e) => Err(Refused::NotANumber(e)),
    }
}

/// Puts `quantity` of the product in the cart: in its line, or as a new one. Adding, the reply
/// offers "สั่งซื้อเลย", "ดูตะกร้า" and "เลือกซื้อต่อ" (FR-CART-01); changing, the cart follows.
async fn put(turn: &Turn<'_>, room: &Room, quantity: i32) -> Result<Vec<Message>, DbErr> {
    let product = &room.product;
    let held = room.after(quantity);
    let line = cart_item::ActiveModel {
        user_id: Set(turn.actor.user.id),
        product_id: Set(product.id),
        quantity: Set(held),
        added_at: Set(turn.now),
    };
    // A line already there keeps when it was added, so the cart keeps its order
    cart_item::Entity::insert(line)
        .on_conflict(
            OnConflict::columns([cart_item::Column::UserId, cart_item::Column::ProductId])
                .update_column(cart_item::Column::Quantity)
                .to_owned(),
        )
        .exec_without_returning(turn.db)
        .await?;
    let t = turn.templates;
    let total = product.price.times(held);
    if room.change {
        let changed = t.quantity_changed(&product.name, held, &product.unit, total);
        return Ok(vec![changed, cart(turn, 0).await?]);
    }
    let added = t.added_to_cart(&product.name, held, &product.unit, total, room.in_cart > 0);
    let actions = [
        checkout(product.shop_id, &labels::ORDER_NOW),
        view_cart(),
        continue_shopping(),
    ];
    Ok(vec![added.with_quick_reply(QuickReply::new(actions))])
}

/// The line can take no more: how many are left, and that it holds them already.
fn too_many(turn: &Turn<'_>, room: &Room) -> Message {
    let product = &room.product;
    let full = turn.templates.quantity_too_many(
        &product.name,
        &product.unit,
        product.stock,
        MAX_QUANTITY,
        room.in_cart,
    );
    full.with_quick_reply(QuickReply::new([view_cart(), continue_shopping()]))
}

/// "ลบออก" on product `id`'s line: the cart without it. A line already removed leaves the cart
/// as it is.
async fn remove(turn: &Turn<'_>, id: i64) -> Result<Vec<Message>, DbErr> {
    let removed = cart_item::Entity::delete_many()
        .filter(cart_item::Column::UserId.eq(turn.actor.user.id))
        .filter(cart_item::Column::ProductId.eq(id))
        .exec(turn.db)
        .await?;
    let mut messages = Vec::new();
    if removed.rows_affected > 0
        && let Some(product) = product::Entity::find_by_id(id).one(turn.db).await?
    {
        messages.push(turn.templates.removed_from_cart(&product.name));
    }
    messages.push(cart(turn, 0).await?);
    Ok(messages)
}

/// Page `page` of the cart (FR-CART-03): a card for each shop, [`CART_SHOPS_PER_PAGE`] a page,
/// or that it is empty. A page past the last, from a button sent before lines were removed,
/// starts over.
async fn cart(turn: &Turn<'_>, page: usize) -> Result<Message, DbErr> {
    let lines = lines(turn.db, turn.actor.user.id).await?;
    if lines.is_empty() {
        return Ok(turn.templates.empty_cart(next_actions()));
    }
    let shops: Vec<CartShop<'_>> = lines
        .chunk_by(|a, b| a.shop.id == b.shop.id)
        .map(shop_card)
        .collect();
    let pages = shops.len().div_ceil(CART_SHOPS_PER_PAGE);
    let page = if page < pages { page } else { 0 };
    let more = |next| {
        let more = Item::Cart.postback().with_int(PAGE, next as i64);
        more.to_string()
    };
    Ok(turn.templates.cart(&shops, page, more, next_actions()))
}

/// The card of a shop's `lines` in the cart: its subtotal is that of those that can be ordered,
/// and "สั่งซื้อร้านนี้" checks them out, if any can.
fn shop_card(lines: &[Line]) -> CartShop<'_> {
    let shop = &lines[0].shop;
    let orderable = lines.iter().filter(|line| line.state.orderable());
    CartShop {
        name: &shop.name,
        lines: lines.iter().map(line_card).collect(),
        subtotal: orderable.clone().map(Line::total).sum(),
        excluded: lines.iter().any(|line| !line.state.orderable()),
        checkout: (orderable.count() > 0).then(|| checkout(shop.id, &labels::ORDER_FROM_SHOP)),
    }
}

/// A line as its shop's card in the cart shows it, with "แก้จำนวน" while its product can be
/// ordered, and "ลบออก".
fn line_card(line: &Line) -> CartLine<'_> {
    let id = line.product.id;
    let mut buttons = Vec::new();
    if line.state.changeable() {
        let change = Postback::new(QUANTITY).with_int(orders::PRODUCT_ID, id);
        buttons.push(change.button(labels::CHANGE_QUANTITY.clone()));
    }
    let remove = Postback::new(REMOVE).with_int(orders::PRODUCT_ID, id);
    buttons.push(remove.button(labels::REMOVE.clone()));
    CartLine {
        name: &line.product.name,
        unit: &line.product.unit,
        price: line.product.price,
        quantity: line.item.quantity,
        total: line.total(),
        state: line.state,
        buttons,
    }
}

/// The quantities a line can take, 1 to 5 at most, as Quick Replies sending them as typed, then
/// "ระบุเอง" to type another.
fn quantities(most: i32) -> Vec<Action> {
    let mut choices: Vec<Action> = (1..=most.min(QUICK_QUANTITIES))
        .map(|n| Action::message(Label::fit(n.to_string()), n.to_string()))
        .collect();
    let typed = &labels::TYPE_QUANTITY;
    choices.push(Action::message(typed.clone(), typed.as_str()));
    choices
}

/// "สั่งซื้อเลย" or "สั่งซื้อร้านนี้", labelled `label`: checks out shop `shop_id`'s lines (#32).
fn checkout(shop_id: i64, label: &Label) -> Action {
    let checkout = Postback::new(orders::CHECKOUT).with_int(orders::SHOP_ID, shop_id);
    checkout.button(label.clone())
}

fn view_cart() -> Action {
    Item::Cart.postback().button(labels::VIEW_CART.clone())
}

fn continue_shopping() -> Action {
    Item::Buy
        .postback()
        .button(labels::CONTINUE_SHOPPING.clone())
}

fn menu_button() -> Action {
    Action::message(labels::MENU.clone(), labels::MENU.as_str())
}

/// Where to go on from the cart, or from a product that can't go in it: "เลือกซื้อต่อ",
/// "ค้นหา" and the menu.
fn next_actions() -> Vec<Action> {
    vec![
        continue_shopping(),
        Item::Search.postback().button(labels::SEARCH.clone()),
        menu_button(),
    ]
}

fn next() -> QuickReply {
    QuickReply::new(next_actions())
}
