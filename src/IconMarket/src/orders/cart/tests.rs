//! The cart (FR-CART-01 – FR-CART-03, FR-CART-07): how many is asked with Quick Replies of what
//! fits, tapped or typed, and refused over 99 or over what is left, saying what is left; adding
//! again adds to the line; a cart holds at most 20 products, and none of the user's own shop; what
//! changed before the answer is refused; the cart shows its lines by shop, flagging and leaving
//! out those that can't be ordered; lines are changed and removed; a pre-order says when it is
//! ready (FR-PROD-11). Each event is handled in a
//! transaction committed once it is handled. The `async` tests each start a throwaway
//! PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, IntoActiveModel, QueryOrder,
    TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::ai;
use crate::config::{self, Config, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Bubble, Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::{CART_SHOPS_PER_PAGE, Templates, with_cancel};
use crate::persistence::entity::{session, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

/// The day of [`t0`] in Bangkok
fn today() -> NaiveDate {
    t0().with_timezone(&chrono_tz::Asia::Bangkok).date_naive()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    /// Off, unless a test reads numbers in words (FR-AI-13)
    ai: Gateway,
    /// The owner of [`Setup::shop`]
    seller: i64,
    /// ร้านป้าแดง: approved and open
    shop: i64,
    /// ร้านลุงชม: approved and open
    other_shop: i64,
    /// A user without a shop
    buyer: i64,
    category: i64,
    /// ไข่ไก่เบอร์ 2 of [`Setup::shop`]: ฿110 a แผง, 8 left
    eggs: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let shop = testing::new_shop(seller).insert(&db).await.unwrap().id;
    let other_seller = testing::new_user(OTHER_SELLER)
        .insert(&db)
        .await
        .unwrap()
        .id;
    let other_shop = shop::ActiveModel {
        name: Set("ร้านลุงชม".to_owned()),
        ..testing::new_shop(other_seller)
    };
    let other_shop = other_shop.insert(&db).await.unwrap().id;
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    let category = testing::new_category("ไข่/นม").insert(&db).await.unwrap().id;
    let eggs = testing::new_product(shop, category, "ไข่ไก่เบอร์ 2", Some(8))
        .insert(&db)
        .await
        .unwrap()
        .id;
    let config: Config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        ai: ai::testing::off(),
        seller,
        shop,
        other_shop,
        buyer,
        category,
        eggs,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `now` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh.
    async fn handle_at(
        &self,
        user_id: i64,
        now: DateTimeUtc,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, now)
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished { messages, .. } = turn.finish(messages);
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        messages
    }

    /// User `user_id` taps a button sending `data` at `now`, as routing hands it over.
    async fn taps_at(&self, user_id: i64, data: &str, now: DateTimeUtc) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle_at(user_id, now, async |turn| tapped(turn, &postback).await)
            .await
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.taps_at(user_id, data, t0()).await
    }

    /// The buyer taps "ใส่ตะกร้า" on product `id`.
    async fn adds(&self, id: i64) -> Vec<Message> {
        self.taps(self.buyer, &format!("cart.add?product={id}"))
            .await
    }

    /// User `user_id` answers how many with `text` at `now`.
    async fn types_at(&self, user_id: i64, text: &str, now: DateTimeUtc) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        let mut answered = None;
        self.handle_at(user_id, now, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<QuantityFlow>());
            let session = session.expect("asking how many");
            answered = answer(turn, &self.ai, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered.expect("an answer to text")
    }

    /// The buyer answers how many with `text`.
    async fn types(&self, text: &str) -> Vec<Message> {
        self.types_at(self.buyer, text, t0()).await
    }

    /// The buyer types "ตะกร้า".
    async fn shows(&self) -> Vec<Message> {
        self.handle_at(self.buyer, t0(), async |turn| show(turn).await)
            .await
    }

    /// A product of shop `shop_id` called `name`, at ฿110 a แผง, with `stock`.
    async fn product(&self, shop_id: i64, name: &str, stock: Option<i32>) -> i64 {
        let product = testing::new_product(shop_id, self.category, name, stock);
        product.insert(&self.db).await.unwrap().id
    }

    /// Puts `quantity` of product `id` in user `user_id`'s cart, added at `at`.
    async fn put(&self, user_id: i64, id: i64, quantity: i32, at: DateTimeUtc) {
        let line = cart_item::ActiveModel {
            user_id: Set(user_id),
            product_id: Set(id),
            quantity: Set(quantity),
            added_at: Set(at),
        };
        line.insert(&self.db).await.unwrap();
    }

    /// User `user_id`'s cart: each product and how many, by product ID.
    async fn cart_of(&self, user_id: i64) -> Vec<(i64, i32)> {
        cart_item::Entity::find()
            .filter(cart_item::Column::UserId.eq(user_id))
            .order_by_asc(cart_item::Column::ProductId)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|item| (item.product_id, item.quantity))
            .collect()
    }

    /// The buyer's flow in progress, by name.
    async fn flow(&self) -> Option<String> {
        let session = session::Entity::find_by_id(self.buyer)
            .one(&self.db)
            .await
            .unwrap();
        session.map(|session| session.flow)
    }

    async fn set_stock(&self, id: i64, stock: Option<i32>) {
        product::Entity::update_many()
            .col_expr(product::Column::Stock, Expr::value(stock))
            .filter(product::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    async fn set_status(&self, id: i64, status: product::Status) {
        product::Entity::update_many()
            .col_expr(product::Column::Status, Expr::value(status))
            .filter(product::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Opens or closes shop `id` (FR-SHOP-05).
    async fn set_open(&self, id: i64, is_open: bool) {
        let mut shop = shop::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        shop.is_open = Set(is_open);
        shop.update(&self.db).await.unwrap();
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The labels of `message`'s Quick Reply buttons.
fn quick_replies(message: &Message) -> Vec<&str> {
    let Some(quick_reply) = &message.quick_reply else {
        return Vec::new();
    };
    quick_reply
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
}

/// What `message`'s Quick Reply buttons send: their postback data, or the text they send as
/// typed.
fn quick_reply_data(message: &Message) -> Vec<&str> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| match &item.action {
            Action::Postback { data, .. } => data.as_str(),
            Action::Message { text, .. } => text.as_str(),
            action => panic!("neither postback nor text: {action:?}"),
        })
        .collect()
}

/// The cards of the cart, `message`, and the postback of its "ดูเพิ่มเติม" if it has one.
fn page(message: &Message) -> (Vec<&Bubble>, Option<&str>) {
    let Body::Flex {
        alt_text,
        contents: FlexContainer::Carousel(carousel),
    } = &message.body
    else {
        panic!("not a carousel: {:?}", message.body);
    };
    assert_eq!(alt_text, "ตะกร้า");
    let mut cards: Vec<&Bubble> = carousel.contents.iter().collect();
    let more = match cards.last().map(|card| buttons(card)) {
        Some(buttons) if buttons.len() == 1 && buttons[0].0 == "ดูเพิ่มเติม" =>
        {
            cards.pop();
            Some(match &carousel.contents.last().unwrap().body {
                Some(body) => match &body.contents[0] {
                    Component::Button(button) => match &button.action {
                        Action::Postback { data, .. } => data.as_str(),
                        action => panic!("not a postback: {action:?}"),
                    },
                    component => panic!("not a button: {component:?}"),
                },
                None => panic!("no body"),
            })
        }
        _ => None,
    };
    (cards, more)
}

/// The texts of `card`'s body, in order.
fn texts(card: &Bubble) -> Vec<String> {
    fn texts(component: &Component, out: &mut Vec<String>) {
        match component {
            Component::Text(text) => out.push(text.text.clone()),
            Component::Box(flex_box) => {
                for component in &flex_box.contents {
                    texts(component, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for component in &card.body.as_ref().unwrap().contents {
        texts(component, &mut out);
    }
    out
}

/// The label and postback data of each button of `card`, its body's then its footer's.
fn buttons(card: &Bubble) -> Vec<(String, String)> {
    fn buttons(component: &Component, out: &mut Vec<(String, String)>) {
        match component {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => {
                    out.push((label.as_str().to_owned(), data.clone()));
                }
                action => panic!("not a postback: {action:?}"),
            },
            Component::Box(flex_box) => {
                for component in &flex_box.contents {
                    buttons(component, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for part in [&card.body, &card.footer].into_iter().flatten() {
        for component in &part.contents {
            buttons(component, &mut out);
        }
    }
    out
}

fn pairs(pairs: &[(&str, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(label, data)| ((*label).to_owned(), data.clone()))
        .collect()
}

const ASKED: [&str; 7] = ["1", "2", "3", "4", "5", "ระบุเอง", "ยกเลิก"];
const NEXT: [&str; 3] = ["เลือกซื้อต่อ", "ค้นหา", "เมนู"];

/// FR-CART-01, Appendix A.2: "ใส่ตะกร้า" asks how many, with Quick Replies of 1 to 5, as many as
/// are left, that send their number as typed, "ระบุเอง" and the cancel button.
#[tokio::test]
async fn adding_asks_how_many_with_the_quantities_that_fit() {
    let s = setup().await;
    let asked = s.adds(s.eggs).await;
    assert_eq!(asked.len(), 1);
    assert_eq!(text(&asked[0]), "\"ไข่ไก่เบอร์ 2\" เอากี่แผงคะ");
    assert_eq!(quick_replies(&asked[0]), ASKED);
    assert_eq!(quick_reply_data(&asked[0]), ASKED);
    assert_eq!(s.flow().await.as_deref(), Some(QuantityFlow::NAME));
    assert_eq!(s.cart_of(s.buyer).await, []);

    let three = s.product(s.shop, "ไข่เป็ด", Some(3)).await;
    let asked = s.adds(three).await;
    assert_eq!(quick_replies(&asked[0]), ["1", "2", "3", "ระบุเอง", "ยกเลิก"]);

    let unlimited = s.product(s.shop, "ขนมครก", None).await;
    assert_eq!(quick_replies(&s.adds(unlimited).await[0]), ASKED);
}

/// FR-CART-01, FR-CART-02, Appendix A.2: the quantity tapped goes in the cart, which says so with
/// the line's total and offers "สั่งซื้อเลย" for the shop, "ดูตะกร้า" and "เลือกซื้อต่อ".
#[tokio::test]
async fn a_tapped_quantity_goes_in_the_cart() {
    let s = setup().await;
    s.adds(s.eggs).await;
    let added = s.types("2").await;
    assert_eq!(added.len(), 1);
    assert_eq!(text(&added[0]), "ใส่ตะกร้าแล้วค่ะ ไข่ไก่เบอร์ 2 × 2 แผง = ฿220");
    assert_eq!(quick_replies(&added[0]), ["สั่งซื้อเลย", "ดูตะกร้า", "เลือกซื้อต่อ"]);
    let checkout = format!("checkout?shop={}", s.shop);
    assert_eq!(
        quick_reply_data(&added[0]),
        [checkout.as_str(), "cart", "buy"]
    );
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);
    assert_eq!(s.flow().await, None);
}

/// FR-CART-01, FR-UI-06: "ระบุเอง" asks to type the quantity, which may be in Thai digits or
/// followed by the unit; adding again adds to the line.
#[tokio::test]
async fn a_quantity_can_be_typed() {
    let s = setup().await;
    let t = &s.templates;
    s.adds(s.eggs).await;
    let asked = s.types("ระบุเอง").await;
    assert_eq!(asked, [with_cancel(t.ask_typed_quantity(8, "แผง"))]);
    assert_eq!(quick_replies(&asked[0]), ["ยกเลิก"]);
    assert_eq!(s.flow().await.as_deref(), Some(QuantityFlow::NAME));
    s.types("๓").await;
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 3)]);

    let asked = s.adds(s.eggs).await;
    assert_eq!(
        text(&asked[0]),
        "\"ไข่ไก่เบอร์ 2\" อยู่ในตะกร้าแล้ว 3 แผง เอาเพิ่มอีกกี่แผงคะ"
    );
    let added = s.types("2 แผง").await;
    assert_eq!(
        text(&added[0]),
        "ใส่เพิ่มแล้วค่ะ ตอนนี้ในตะกร้ามี ไข่ไก่เบอร์ 2 × 5 แผง = ฿550"
    );
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 5)]);
}

/// FR-CART-01: a line holds 1 to 99; anything else is refused, and the question waits.
#[tokio::test]
async fn quantities_other_than_1_to_99_are_refused() {
    let s = setup().await;
    let t = &s.templates;
    let unlimited = s.product(s.shop, "ขนมครก", None).await;
    s.adds(unlimited).await;
    let choices = |message: Message| with_choices(vec![message], quantities(99));
    let refusals = [
        ("0", choices(t.quantity_too_small("แผง"))),
        (
            "100",
            choices(t.quantity_too_many("ขนมครก", "แผง", None, 99, 0)),
        ),
        (
            "99999999999",
            choices(t.quantity_too_many("ขนมครก", "แผง", None, 99, 0)),
        ),
        (
            "สองแผง",
            choices(Message::text(t.number_problem(NumberError::NotANumber))),
        ),
        (
            "2.5",
            choices(Message::text(t.number_problem(NumberError::NotWhole))),
        ),
    ];
    for (typed, refused) in refusals {
        let mut refused = refused;
        let last = refused.pop().unwrap();
        refused.push(with_cancel(last));
        assert_eq!(s.types(typed).await, refused, "{typed}");
        assert_eq!(s.flow().await.as_deref(), Some(QuantityFlow::NAME));
    }
    // Nothing at all asks again
    let asked = s.types("  ").await;
    assert_eq!(text(&asked[0]), "\"ขนมครก\" เอากี่แผงคะ");
    assert_eq!(s.cart_of(s.buyer).await, []);

    s.types("99").await;
    assert_eq!(s.cart_of(s.buyer).await, [(unlimited, 99)]);
    assert_eq!(s.flow().await, None);
}

/// Acceptance, FR-CART-01: a quantity over what is left is refused, saying how many are left,
/// and the cart is unchanged.
#[tokio::test]
async fn more_than_is_left_is_refused_with_what_is_left() {
    let s = setup().await;
    s.adds(s.eggs).await;
    let refused = s.types("9").await;
    assert_eq!(refused.len(), 1);
    assert_eq!(
        text(&refused[0]),
        "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 8 แผง พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&refused[0]), ASKED);
    assert_eq!(s.cart_of(s.buyer).await, []);
    assert_eq!(s.flow().await.as_deref(), Some(QuantityFlow::NAME));

    s.types("8").await;
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 8)]);
}

/// FR-CART-01: adding to a line counts what it holds: only what is left beyond it is offered
/// and taken, and once it holds all that is left, nothing more is asked.
#[tokio::test]
async fn adding_again_takes_only_what_is_left_beyond_the_line() {
    let s = setup().await;
    s.put(s.buyer, s.eggs, 5, t0()).await;
    let asked = s.adds(s.eggs).await;
    assert_eq!(quick_replies(&asked[0]), ["1", "2", "3", "ระบุเอง", "ยกเลิก"]);
    let refused = s.types("4").await;
    assert_eq!(
        text(&refused[0]),
        "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 8 แผง ในตะกร้ามีแล้ว 5 แผง จึงใส่เพิ่มได้อีกไม่เกิน 3 แผง พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลยค่ะ"
    );
    s.types("3").await;
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 8)]);

    let full = s.adds(s.eggs).await;
    assert_eq!(full.len(), 1);
    assert_eq!(
        text(&full[0]),
        "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 8 แผง ในตะกร้ามีแล้ว 8 แผง จึงใส่เพิ่มไม่ได้แล้วค่ะ"
    );
    assert_eq!(quick_replies(&full[0]), ["ดูตะกร้า", "เลือกซื้อต่อ"]);
    assert_eq!(s.flow().await, None);
}

/// FR-CART-07: a cart holds at most 20 products, as many as an order may; a line already in it
/// still takes more.
#[tokio::test]
async fn a_cart_holds_at_most_20_products() {
    let s = setup().await;
    let t = &s.templates;
    let mut ids = Vec::new();
    for i in 0..=MAX_ITEMS {
        ids.push(s.product(s.other_shop, &format!("ขนม {i}"), None).await);
    }
    for &id in &ids[..MAX_ITEMS] {
        s.put(s.buyer, id, 1, t0()).await;
    }
    let full = s.adds(ids[MAX_ITEMS]).await;
    assert_eq!(full.len(), 1);
    assert_eq!(text(&full[0]), text(&t.cart_full(20)));
    assert_eq!(quick_replies(&full[0]), ["ดูตะกร้า", "เมนู"]);
    assert_eq!(s.flow().await, None);

    s.adds(ids[0]).await;
    s.types("2").await;
    assert_eq!(s.cart_of(s.buyer).await.len(), MAX_ITEMS);
    assert_eq!(s.cart_of(s.buyer).await[0], (ids[0], 3));
}

/// FR-CART-07: a seller can't put their own shop's products in their cart, only other shops'.
#[tokio::test]
async fn a_seller_cant_add_their_own_products() {
    let s = setup().await;
    let t = &s.templates;
    let own = s
        .taps(s.seller, &format!("cart.add?product={}", s.eggs))
        .await;
    assert_eq!(own, [t.own_product().with_quick_reply(next())]);
    assert_eq!(quick_replies(&own[0]), NEXT);
    assert_eq!(s.cart_of(s.seller).await, []);

    let other = s.product(s.other_shop, "ขนมครก", None).await;
    let asked = s.taps(s.seller, &format!("cart.add?product={other}")).await;
    assert_eq!(text(&asked[0]), "\"ขนมครก\" เอากี่แผงคะ");
}

/// FR-UI-13, FR-PROD-06, FR-SHOP-05: a product that can't be ordered can't be put in the cart:
/// sold out, of a closed shop, or no longer seen by buyers.
#[tokio::test]
async fn what_cant_be_ordered_cant_be_added() {
    let s = setup().await;
    let t = &s.templates;
    s.set_stock(s.eggs, Some(0)).await;
    let refused = s.adds(s.eggs).await;
    assert_eq!(
        refused,
        [
            t.not_orderable("ไข่ไก่เบอร์ 2", "ร้านป้าแดง", Availability::SoldOut)
                .with_quick_reply(next())
        ]
    );
    s.set_stock(s.eggs, Some(8)).await;
    s.set_open(s.shop, false).await;
    let refused = s.adds(s.eggs).await;
    assert_eq!(
        text(&refused[0]),
        "ขอโทษค่ะ ตอนนี้ร้านป้าแดงปิดอยู่ จึงใส่ \"ไข่ไก่เบอร์ 2\" ลงตะกร้าไม่ได้ ลองใหม่ตอนร้านเปิด หรือดูสินค้าร้านอื่นได้เลยค่ะ"
    );
    s.set_open(s.shop, true).await;
    s.set_status(s.eggs, product::Status::Hidden).await;
    assert_eq!(s.adds(s.eggs).await, [t.denied(Denied::NotFound)]);
    assert_eq!(s.adds(999_999).await, [t.denied(Denied::NotFound)]);
    assert_eq!(s.flow().await, None);
    assert_eq!(s.cart_of(s.buyer).await, []);
}

/// FR-UI-13: what changed between the question and the answer is found at the answer: a closed
/// shop, a sold-out or hidden product end it, and fewer left refuse more than that.
#[tokio::test]
async fn what_changed_before_the_answer_is_refused() {
    let s = setup().await;
    let t = &s.templates;
    s.adds(s.eggs).await;
    s.set_open(s.shop, false).await;
    let closed = t.not_orderable("ไข่ไก่เบอร์ 2", "ร้านป้าแดง", Availability::ShopClosed);
    assert_eq!(s.types("2").await, [closed.with_quick_reply(next())]);
    assert_eq!(s.flow().await, None);
    s.set_open(s.shop, true).await;

    s.adds(s.eggs).await;
    s.set_stock(s.eggs, Some(0)).await;
    let sold_out = t.not_orderable("ไข่ไก่เบอร์ 2", "ร้านป้าแดง", Availability::SoldOut);
    assert_eq!(s.types("2").await, [sold_out.with_quick_reply(next())]);
    s.set_stock(s.eggs, Some(8)).await;

    s.adds(s.eggs).await;
    s.set_status(s.eggs, product::Status::Hidden).await;
    assert_eq!(s.types("2").await, [t.denied(Denied::NotFound)]);
    s.set_status(s.eggs, product::Status::Active).await;

    s.adds(s.eggs).await;
    s.set_stock(s.eggs, Some(1)).await;
    let refused = s.types("2").await;
    assert!(text(&refused[0]).contains("เหลือ 1 แผง"), "{refused:?}");
    assert_eq!(quick_replies(&refused[0]), ["1", "ระบุเอง", "ยกเลิก"]);
    assert_eq!(s.cart_of(s.buyer).await, []);
    s.types("1").await;
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 1)]);

    // Fewer left than the line holds already: nothing more fits, whatever is typed
    s.set_stock(s.eggs, Some(3)).await;
    s.adds(s.eggs).await;
    s.set_stock(s.eggs, Some(1)).await;
    let full = s.types("ระบุเอง").await;
    assert_eq!(
        text(&full[0]),
        "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 1 แผง ในตะกร้ามีแล้ว 1 แผง จึงใส่เพิ่มไม่ได้แล้วค่ะ"
    );
    assert_eq!(s.flow().await, None);
}

/// FR-ADM-04, NFR-SEC-10: a suspended user can't use the cart, and a question in progress ends.
#[tokio::test]
async fn suspended_users_cant_use_the_cart() {
    let s = setup().await;
    let t = &s.templates;
    s.put(s.buyer, s.eggs, 2, t0()).await;
    s.adds(s.eggs).await;
    let mut buyer = user::Entity::find_by_id(s.buyer)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    buyer.status = Set(user::Status::Suspended);
    buyer.update(&s.db).await.unwrap();
    let denied = [t.denied(Denied::Suspended)];
    assert_eq!(s.types("1").await, denied);
    assert_eq!(s.flow().await, None);
    assert_eq!(s.adds(s.eggs).await, denied);
    assert_eq!(s.taps(s.buyer, "cart").await, denied);
    assert_eq!(s.shows().await, denied);
    let remove = format!("cart.remove?product={}", s.eggs);
    assert_eq!(s.taps(s.buyer, &remove).await, denied);
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);
}

/// FR-ADM-04: a suspended seller's shop takes no orders: buyers can't add its products, and
/// lines already in a cart show it closed, until the suspension is lifted.
#[tokio::test]
async fn a_suspended_sellers_products_cant_be_ordered() {
    let s = setup().await;
    let t = &s.templates;
    let fine = s.product(s.other_shop, "ขนมกล้วย", None).await;
    s.put(s.buyer, s.eggs, 2, t0()).await;
    s.put(s.buyer, fine, 1, t0() + TimeDelta::minutes(1)).await;
    let suspend = |status| {
        user::Entity::update_many()
            .col_expr(user::Column::Status, Expr::value(status))
            .filter(user::Column::Id.eq(s.seller))
            .exec(&s.db)
    };
    suspend(user::Status::Suspended).await.unwrap();
    assert_eq!(s.adds(s.eggs).await, [t.denied(Denied::NotFound)]);
    let states: Vec<_> = lines(&s.db, s.buyer, today())
        .await
        .unwrap()
        .iter()
        .map(|line| (line.product.id, line.state))
        .collect();
    assert_eq!(
        states,
        [(fine, State::Orderable), (s.eggs, State::ShopClosed)]
    );

    suspend(user::Status::Active).await.unwrap();
    let states: Vec<_> = lines(&s.db, s.buyer, today())
        .await
        .unwrap()
        .iter()
        .map(|line| line.state)
        .collect();
    assert_eq!(states, [State::Orderable, State::Orderable]);
}

/// FR-CART-03: "ตะกร้า" shows a card per shop, the shop added to last first, each line's name,
/// quantity and unit price, and total, with "แก้จำนวน" and "ลบออก", then the shop's subtotal and
/// "สั่งซื้อร้านนี้"; typed, the same.
#[tokio::test]
async fn the_cart_shows_its_lines_by_shop() {
    let s = setup().await;
    let duck_eggs = s.product(s.shop, "ไข่เป็ด", Some(5)).await;
    let snacks = s.product(s.other_shop, "ขนมครก", None).await;
    let minute = TimeDelta::minutes(1);
    s.put(s.buyer, s.eggs, 2, t0()).await;
    s.put(s.buyer, snacks, 3, t0() + minute).await;
    s.put(s.buyer, duck_eggs, 1, t0() + minute * 2).await;

    let shown = s.taps(s.buyer, "cart").await;
    assert_eq!(shown.len(), 1);
    assert_eq!(quick_replies(&shown[0]), NEXT);
    let (cards, more) = page(&shown[0]);
    assert_eq!(more, None);
    assert_eq!(cards.len(), 2);
    assert_eq!(
        texts(cards[0]),
        [
            "ร้านป้าแดง",
            "ไข่ไก่เบอร์ 2",
            "2 แผง × ฿110",
            "฿220",
            "ไข่เป็ด",
            "1 แผง × ฿110",
            "฿110",
            "รวม",
            "฿330",
        ]
    );
    let line = |id: i64| {
        [
            ("แก้จำนวน", format!("cart.quantity?product={id}")),
            ("ลบออก", format!("cart.remove?product={id}")),
        ]
    };
    let mut expected = Vec::new();
    expected.extend(line(s.eggs));
    expected.extend(line(duck_eggs));
    expected.push(("สั่งซื้อร้านนี้", format!("checkout?shop={}", s.shop)));
    assert_eq!(buttons(cards[0]), pairs(&expected));
    assert_eq!(
        texts(cards[1]),
        ["ร้านลุงชม", "ขนมครก", "3 แผง × ฿110", "฿330", "รวม", "฿330"]
    );
    assert_eq!(s.shows().await, shown);
    assert_eq!(s.flow().await, None);
}

/// FR-PROD-11: a pre-order says when it is ready when asked how many, once in the cart, and on
/// its line there.
#[tokio::test]
async fn a_pre_order_says_when_it_is_ready_in_the_cart() {
    let s = setup().await;
    let mooncakes = testing::new_product(s.shop, s.category, "ขนมไหว้พระจันทร์", None);
    let mooncakes = product::ActiveModel {
        ready_on: Set(NaiveDate::from_ymd_opt(2026, 11, 5)),
        ..mooncakes
    };
    let mooncakes = mooncakes.insert(&s.db).await.unwrap().id;

    let asked = s.adds(mooncakes).await;
    assert_eq!(
        text(&asked[0]),
        "\"ขนมไหว้พระจันทร์\" (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569) เอากี่แผงคะ"
    );
    let added = s.types("2").await;
    assert_eq!(
        text(&added[0]),
        "ใส่ตะกร้าแล้วค่ะ ขนมไหว้พระจันทร์ × 2 แผง = ฿220 (สั่งล่วงหน้า พร้อม 5 พ.ย. 2569)"
    );
    s.put(s.buyer, s.eggs, 1, t0()).await;
    let shown = s.shows().await;
    let (cards, _) = page(&shown[0]);
    assert_eq!(
        texts(cards[0]),
        [
            "ร้านป้าแดง",
            "ไข่ไก่เบอร์ 2",
            "1 แผง × ฿110",
            "฿110",
            "ขนมไหว้พระจันทร์",
            "2 แผง × ฿110",
            "฿220",
            "สั่งล่วงหน้า · พร้อม 5 พ.ย. 2569",
            "รวม",
            "฿330",
        ]
    );
    let [eggs, line] = &lines(&s.db, s.buyer, today()).await.unwrap()[..] else {
        panic!("two lines");
    };
    assert_eq!(eggs.ready_on, None);
    assert_eq!(line.ready_on, NaiveDate::from_ymd_opt(2026, 11, 5));
    // From its ready day on, it is no pre-order
    let the_day = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
    let lines = lines(&s.db, s.buyer, the_day).await.unwrap();
    assert!(lines.iter().all(|line| line.ready_on.is_none()));
}

/// Acceptance, FR-CART-03: a line whose shop closed after it was added is flagged, as are those
/// hidden, sold out or with fewer left than it holds; they are left out of the subtotal, and
/// only "ลบออก" is offered but where fewer are left. A shop with no line that can be ordered has
/// no "สั่งซื้อร้านนี้".
#[tokio::test]
async fn lines_that_cant_be_ordered_are_flagged_and_left_out() {
    let s = setup().await;
    let hidden = s.product(s.other_shop, "ขนมชั้น", None).await;
    let sold_out = s.product(s.other_shop, "ขนมครก", Some(2)).await;
    let short = s.product(s.other_shop, "ขนมถ้วย", Some(9)).await;
    let fine = s.product(s.other_shop, "ขนมกล้วย", None).await;
    s.put(s.buyer, s.eggs, 2, t0()).await;
    for id in [hidden, sold_out, short, fine] {
        s.put(s.buyer, id, 3, t0() + TimeDelta::minutes(1)).await;
    }
    s.set_open(s.shop, false).await;
    s.set_status(hidden, product::Status::Hidden).await;
    s.set_stock(sold_out, Some(0)).await;
    s.set_stock(short, Some(1)).await;

    let shown = s.shows().await;
    let (cards, _) = page(&shown[0]);
    assert_eq!(cards.len(), 2);
    assert_eq!(
        texts(cards[0]),
        [
            "ร้านลุงชม",
            "ขนมชั้น",
            "3 แผง × ฿110",
            "฿330",
            "ตอนนี้ไม่มีขาย",
            "ขนมครก",
            "3 แผง × ฿110",
            "฿330",
            "หมด",
            "ขนมถ้วย",
            "3 แผง × ฿110",
            "฿330",
            "เหลือแค่ 1 แผง",
            "ขนมกล้วย",
            "3 แผง × ฿110",
            "฿330",
            "รวม",
            "฿330",
            "ไม่รวมรายการที่สั่งไม่ได้",
        ]
    );
    let remove = |id: i64| ("ลบออก", format!("cart.remove?product={id}"));
    let change = |id: i64| ("แก้จำนวน", format!("cart.quantity?product={id}"));
    assert_eq!(
        buttons(cards[0]),
        pairs(&[
            remove(hidden),
            remove(sold_out),
            change(short),
            remove(short),
            change(fine),
            remove(fine),
            ("สั่งซื้อร้านนี้", format!("checkout?shop={}", s.other_shop)),
        ])
    );
    // Its shop closed after it was added
    assert_eq!(
        texts(cards[1]),
        [
            "ร้านป้าแดง",
            "ไข่ไก่เบอร์ 2",
            "2 แผง × ฿110",
            "฿220",
            "ร้านปิด",
            "รวม",
            "฿0",
            "ไม่รวมรายการที่สั่งไม่ได้",
        ]
    );
    assert_eq!(buttons(cards[1]), pairs(&[remove(s.eggs)]));
    assert_eq!(cards[1].footer, None);

    // The states #32 checks out by
    let lines = lines(&s.db, s.buyer, today()).await.unwrap();
    let states: Vec<_> = lines.iter().map(|line| line.state).collect();
    assert_eq!(
        states,
        [
            State::Unavailable,
            State::SoldOut,
            State::NotEnough(1),
            State::Orderable,
            State::ShopClosed,
        ]
    );
}

/// FR-CART-03, FR-LINE-11: a cart of more shops than a page holds is paged with "ดูเพิ่มเติม";
/// a page past the last starts over.
#[tokio::test]
async fn a_long_cart_is_paged() {
    let s = setup().await;
    let mut shops = vec![s.shop, s.other_shop];
    for i in 0..(CART_SHOPS_PER_PAGE - 1) {
        let owner = testing::new_user(&format!("U{i:032}"))
            .insert(&s.db)
            .await
            .unwrap();
        let shop = shop::ActiveModel {
            name: Set(format!("ร้าน {i}")),
            ..testing::new_shop(owner.id)
        };
        shops.push(shop.insert(&s.db).await.unwrap().id);
    }
    for (i, &shop) in shops.iter().enumerate() {
        let id = s.product(shop, &format!("ขนม {i}"), None).await;
        let at = t0() + TimeDelta::minutes(i64::try_from(i).unwrap());
        s.put(s.buyer, id, 1, at).await;
    }
    let first = s.taps(s.buyer, "cart").await;
    let (cards, more) = page(&first[0]);
    assert_eq!(cards.len(), CART_SHOPS_PER_PAGE);
    assert_eq!(more, Some("cart?page=1"));
    // The shop added to last first
    assert_eq!(texts(cards[0])[1], format!("ขนม {}", shops.len() - 1));

    let second = s.taps(s.buyer, "cart?page=1").await;
    let (cards, more) = page(&second[0]);
    assert_eq!(cards.len(), 1);
    assert_eq!(more, None);
    assert_eq!(texts(cards[0])[0], "ร้านป้าแดง");

    assert_eq!(s.taps(s.buyer, "cart?page=5").await, first);
}

/// FR-CART-03: an empty cart says so, and where to start.
#[tokio::test]
async fn an_empty_cart_says_so() {
    let s = setup().await;
    let shown = s.taps(s.buyer, "cart").await;
    assert_eq!(shown, [s.templates.empty_cart(next_actions())]);
    assert_eq!(quick_replies(&shown[0]), NEXT);
    assert_eq!(s.shows().await, shown);
}

/// FR-CART-03: "แก้จำนวน" asks how many the line holds now, as many as are left, and shows the
/// cart once changed; a line no longer in the cart is added to it instead.
#[tokio::test]
async fn a_lines_quantity_can_be_changed() {
    let s = setup().await;
    let t = &s.templates;
    s.put(s.buyer, s.eggs, 6, t0()).await;
    s.set_stock(s.eggs, Some(3)).await;
    let shown = s.shows().await;
    let (cards, _) = page(&shown[0]);
    assert!(texts(cards[0]).contains(&"เหลือแค่ 3 แผง".to_owned()));

    let change = format!("cart.quantity?product={}", s.eggs);
    let asked = s.taps(s.buyer, &change).await;
    assert_eq!(
        text(&asked[0]),
        "\"ไข่ไก่เบอร์ 2\" ในตะกร้ามี 6 แผง เปลี่ยนเป็นกี่แผงคะ"
    );
    assert_eq!(quick_replies(&asked[0]), ["1", "2", "3", "ระบุเอง", "ยกเลิก"]);
    let refused = s.types("4").await;
    assert_eq!(
        text(&refused[0]),
        "ขอโทษค่ะ \"ไข่ไก่เบอร์ 2\" เหลือ 3 แผง พิมพ์ใหม่หรือเลือกจากปุ่มด้านล่างได้เลยค่ะ"
    );
    let changed = s.types("3").await;
    assert_eq!(changed.len(), 2);
    assert_eq!(
        changed[0],
        t.quantity_changed("ไข่ไก่เบอร์ 2", 3, "แผง", Money::from_baht(330), None)
    );
    let (cards, _) = page(&changed[1]);
    assert_eq!(
        texts(cards[0])[2..],
        ["3 แผง × ฿110", "฿330", "รวม", "฿330"]
    );
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 3)]);
    assert_eq!(s.flow().await, None);

    // Removed meanwhile: added again
    cart_item::Entity::delete_many().exec(&s.db).await.unwrap();
    let asked = s.taps(s.buyer, &change).await;
    assert_eq!(text(&asked[0]), "\"ไข่ไก่เบอร์ 2\" เอากี่แผงคะ");
    let added = s.types("1").await;
    assert_eq!(text(&added[0]), "ใส่ตะกร้าแล้วค่ะ ไข่ไก่เบอร์ 2 × 1 แผง = ฿110");
}

/// FR-CART-02, FR-CART-03: "ลบออก" removes the line and shows the cart; tapped again, it shows
/// the cart as it is.
#[tokio::test]
async fn removing_a_line_can_be_tapped_again() {
    let s = setup().await;
    let t = &s.templates;
    let snacks = s.product(s.other_shop, "ขนมครก", None).await;
    s.put(s.buyer, s.eggs, 2, t0()).await;
    s.put(s.buyer, snacks, 1, t0()).await;
    // Another buyer's cart stays as it is
    s.put(s.seller, snacks, 4, t0()).await;

    let remove = format!("cart.remove?product={}", s.eggs);
    let removed = s.taps(s.buyer, &remove).await;
    assert_eq!(removed.len(), 2);
    assert_eq!(removed[0], t.removed_from_cart("ไข่ไก่เบอร์ 2"));
    let (cards, _) = page(&removed[1]);
    assert_eq!(cards.len(), 1);
    assert_eq!(s.cart_of(s.buyer).await, [(snacks, 1)]);

    let again = s.taps(s.buyer, &remove).await;
    assert_eq!(again, removed[1..]);

    let emptied = s
        .taps(s.buyer, &format!("cart.remove?product={snacks}"))
        .await;
    assert_eq!(
        emptied,
        [t.removed_from_cart("ขนมครก"), t.empty_cart(next_actions())]
    );
    assert_eq!(s.cart_of(s.buyer).await, []);
    assert_eq!(s.cart_of(s.seller).await, [(snacks, 4)]);
}

/// A quantity is a whole number of at least 1, which may be followed by the unit.
#[test]
fn a_quantity_is_a_whole_number_from_1() {
    assert_eq!(quantity("2", "แผง"), Ok(2));
    assert_eq!(quantity("๑๒", "แผง"), Ok(12));
    assert_eq!(quantity("2 แผง", "แผง"), Ok(2));
    assert_eq!(quantity("2แผง", "แผง"), Ok(2));
    assert_eq!(quantity("1,000", "แผง"), Ok(1_000));
    assert_eq!(quantity("99999999999", "แผง"), Ok(i32::MAX));
    assert_eq!(quantity("0", "แผง"), Err(Refused::Zero));
    assert_eq!(quantity("แผง", "แผง"), Err(Refused::Empty));
    assert_eq!(
        quantity("2.5", "แผง"),
        Err(Refused::NotANumber(NumberError::NotWhole))
    );
    assert_eq!(
        quantity("สอง", "แผง"),
        Err(Refused::NotANumber(NumberError::NotANumber))
    );
}

// The AI assistant's proposals (FR-AI-03)

impl Setup {
    /// What proposing `quantity` of product `id` for user `user_id`'s cart offers.
    async fn offers(&self, user_id: i64, id: i64, quantity: i32) -> Result<Offer, Declined> {
        let mut offered = None;
        self.handle_at(user_id, t0(), async |turn| {
            offered = Some(AddToCart::offer(turn, id, quantity).await?);
            Ok(Vec::new())
        })
        .await;
        offered.unwrap()
    }

    /// User `user_id` taps the confirm button `confirm` at `now`.
    async fn confirms_at(&self, user_id: i64, confirm: &Action, now: DateTimeUtc) -> Vec<Message> {
        let Action::Postback { data, .. } = confirm else {
            panic!("not a postback: {confirm:?}");
        };
        let postback = Postback::parse(data).unwrap();
        self.handle_at(user_id, now, async |turn| {
            pending::tapped(turn, &postback).await
        })
        .await
    }
}

/// AC-04, Appendix A.1: the assistant's "ใส่ 2 แผง" changes nothing until tapped; the tap puts 2
/// in the cart at the price the card showed, as "ใส่ตะกร้า" would, and a second tap adds nothing.
#[tokio::test]
async fn a_proposed_quantity_goes_in_the_cart_once_confirmed() {
    let s = setup().await;
    let offer = s.offers(s.buyer, s.eggs, 2).await.expect("an offer");
    assert_eq!(offer.product.id, s.eggs);
    assert_eq!(offer.shop.id, s.shop);
    assert_eq!(offer.confirm.label().as_str(), "ใส่ 2 แผง");
    assert_eq!(s.cart_of(s.buyer).await, []);
    assert_eq!(s.flow().await, None);

    let added = s.confirms_at(s.buyer, &offer.confirm, t0()).await;
    assert_eq!(text(&added[0]), "ใส่ตะกร้าแล้วค่ะ ไข่ไก่เบอร์ 2 × 2 แผง = ฿220");
    assert_eq!(quick_replies(&added[0]), ["สั่งซื้อเลย", "ดูตะกร้า", "เลือกซื้อต่อ"]);
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);

    let again = s.confirms_at(s.buyer, &offer.confirm, t0()).await;
    assert_eq!(
        text(&again[0]),
        "ใส่ \"ไข่ไก่เบอร์ 2\" ลงตะกร้าไปแล้วค่ะ ดูตะกร้าหรือเลือกซื้อต่อได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&again[0]), ["ดูตะกร้า", "เลือกซื้อต่อ"]);
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);

    // Another, for a product in the cart already, adds to its line, and taps arriving together
    // apply once (NFR-REL-02)
    let more = s.offers(s.buyer, s.eggs, 3).await.expect("an offer");
    let tap = || s.confirms_at(s.buyer, &more.confirm, t0());
    let (one, other) = tokio::join!(tap(), tap());
    let mut replies = [text(&one[0]), text(&other[0])];
    replies.sort();
    assert_eq!(
        replies,
        [
            "ใส่ \"ไข่ไก่เบอร์ 2\" ลงตะกร้าไปแล้วค่ะ ดูตะกร้าหรือเลือกซื้อต่อได้เลยค่ะ",
            "ใส่เพิ่มแล้วค่ะ ตอนนี้ในตะกร้ามี ไข่ไก่เบอร์ 2 × 5 แผง = ฿550",
        ]
    );
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 5)]);

    // Someone else's button is not theirs to tap (FR-UI-13)
    let other = s.offers(s.buyer, s.eggs, 1).await.expect("an offer");
    let gone = s.confirms_at(s.seller, &other.confirm, t0()).await;
    assert!(text(&gone[0]).contains("หมดเวลาแล้ว"), "{gone:?}");
    assert_eq!(s.cart_of(s.seller).await, []);
}

/// FR-UI-13: a product that changed since its proposal isn't put in the cart: at a new price the
/// proposal is made again at it, from the database (FR-AI-04); with fewer left, or no longer
/// orderable, the tap says why; an expired button says so.
#[tokio::test]
async fn a_proposal_whose_product_changed_shows_it_as_it_is() {
    let s = setup().await;
    let t = &s.templates;
    let offer = s.offers(s.buyer, s.eggs, 2).await.expect("an offer");
    product::Entity::update_many()
        .col_expr(product::Column::Price, Expr::value(Money::from_baht(115)))
        .filter(product::Column::Id.eq(s.eggs))
        .exec(&s.db)
        .await
        .unwrap();
    let changed = s.confirms_at(s.buyer, &offer.confirm, t0()).await;
    assert_eq!(
        text(&changed[0]),
        "\"ไข่ไก่เบอร์ 2\" มีการเปลี่ยนแปลงค่ะ ตอนนี้ราคา ฿115 / แผง ใส่ 2 แผง รวม ฿230 ไหมคะ"
    );
    assert_eq!(
        quick_replies(&changed[0]),
        ["ใส่ 2 แผง", "ดูตะกร้า", "เลือกซื้อต่อ"]
    );
    assert_eq!(s.cart_of(s.buyer).await, []);
    let quick_reply = changed[0].quick_reply.as_ref().unwrap();
    let again = quick_reply.items[0].action.clone();
    let added = s.confirms_at(s.buyer, &again, t0()).await;
    assert_eq!(text(&added[0]), "ใส่ตะกร้าแล้วค่ะ ไข่ไก่เบอร์ 2 × 2 แผง = ฿230");

    // Fewer left than proposed: how many fit now is asked
    let offer = s.offers(s.buyer, s.eggs, 5).await.expect("an offer");
    s.set_stock(s.eggs, Some(4)).await;
    let asked = s.confirms_at(s.buyer, &offer.confirm, t0()).await;
    assert_eq!(
        text(&asked[0]),
        "\"ไข่ไก่เบอร์ 2\" อยู่ในตะกร้าแล้ว 2 แผง เอาเพิ่มอีกกี่แผงคะ"
    );
    assert_eq!(quick_replies(&asked[0]), ["1", "2", "ระบุเอง", "ยกเลิก"]);
    assert_eq!(s.flow().await.as_deref(), Some("cart_quantity"));
    s.handle_at(s.buyer, t0(), async |turn| flow::cancel(turn).await)
        .await;
    let offer = s.offers(s.buyer, s.eggs, 1).await.expect("an offer");
    s.set_open(s.shop, false).await;
    let closed = t.not_orderable("ไข่ไก่เบอร์ 2", "ร้านป้าแดง", Availability::ShopClosed);
    let refused = s.confirms_at(s.buyer, &offer.confirm, t0()).await;
    assert_eq!(refused, [closed.with_quick_reply(next())]);
    s.set_open(s.shop, true).await;
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);

    let offer = s.offers(s.buyer, s.eggs, 1).await.expect("an offer");
    let later = t0() + TimeDelta::minutes(61);
    let expired = s.confirms_at(s.buyer, &offer.confirm, later).await;
    assert_eq!(
        text(&expired[0]),
        "ปุ่มใส่ \"ไข่ไก่เบอร์ 2\" ลงตะกร้าหมดเวลาแล้วค่ะ กด \"ใส่ตะกร้า\" ที่สินค้า หรือถามผู้ช่วยใหม่ได้เลยค่ะ"
    );
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);
}

/// FR-CART-01, FR-CART-07, FR-ADM-04: what "ใส่ตะกร้า" refuses isn't proposed, and why is said
/// to the user and named to the AI.
#[tokio::test]
async fn what_cant_go_in_the_cart_is_not_proposed() {
    let s = setup().await;
    let code = |offered: Result<Offer, Declined>| offered.map(|_| ()).unwrap_err().code();
    assert_eq!(code(s.offers(s.seller, s.eggs, 1).await), "own_shop");
    assert_eq!(code(s.offers(s.buyer, s.eggs, 9).await), "too_many");
    assert_eq!(code(s.offers(s.buyer, s.eggs, 0).await), "too_few");
    assert_eq!(code(s.offers(s.buyer, 999_999, 1).await), "not_found");
    s.set_stock(s.eggs, Some(0)).await;
    assert_eq!(code(s.offers(s.buyer, s.eggs, 1).await), "sold_out");
    s.set_stock(s.eggs, Some(8)).await;
    s.set_open(s.shop, false).await;
    assert_eq!(code(s.offers(s.buyer, s.eggs, 1).await), "shop_closed");
    s.set_open(s.shop, true).await;
    s.put(s.buyer, s.eggs, 6, t0()).await;
    let declined = s.offers(s.buyer, s.eggs, 3).await.map(|_| ());
    assert!(
        matches!(declined, Err(Declined::TooMany(2))),
        "{declined:?}"
    );
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 6)]);
    let mut buyer = user::Entity::find_by_id(s.buyer)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    buyer.status = Set(user::Status::Suspended);
    buyer.update(&s.db).await.unwrap();
    assert_eq!(code(s.offers(s.buyer, s.eggs, 1).await), "not_allowed");
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 6)]);
}
