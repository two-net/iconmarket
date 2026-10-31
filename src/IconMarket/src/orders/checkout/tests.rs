//! Checkout (FR-CART-04 – FR-CART-08, FR-ORD-01, DR-03, DR-07): the steps of Appendix A.2,
//! those with one choice skipped; the address asked for first, changed or shared; the note; the
//! summary and the order it places, numbered by the day in Bangkok, with its stock reserved, its
//! lines copied and its cart lines removed; a repeated tap; what changed since the summary,
//! a changed price among it (AC-12), or a pre-order's ready date (FR-PROD-11); refusals; and checkouts of the last units at the same
//! moment (AC-03, NFR-MNT-07). Each event is handled in a transaction committed once it is
//! handled. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{NaiveDate, TimeDelta, TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder, TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::dispatcher::{Finished, pending};
use crate::line::flex::{Bubble, Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::Templates;
use crate::persistence::entity::{
    cart_item, order, order_event, order_item, pending_action, product, session,
};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;
use crate::users::address::AddressFlow;

static SESSION: config::SessionConfig = config::SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const OTHER_SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const OTHER_BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";
const NEWCOMER: &str = "Ue5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

fn minutes(minutes: i64) -> TimeDelta {
    TimeDelta::minutes(minutes)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    config: Config,
    seller: i64,
    /// ร้านป้าแดง: approved and open, picked up or delivered for ฿10, with a PromptPay ID
    shop: i64,
    /// ร้านลุงชม: approved and open
    other_shop: i64,
    /// Users without a shop, at ซอย 3 บ้านเลขที่ 45/2
    buyer: i64,
    other_buyer: i64,
    category: i64,
    /// ไข่ไก่เบอร์ 2 of [`Setup::shop`]: ฿110 a แผง, 8 left
    eggs: i64,
}

/// `user` at ซอย 3 บ้านเลขที่ 45/2.
fn housed(user: user::ActiveModel) -> user::ActiveModel {
    user::ActiveModel {
        zone: Set(Some("ซอย 3".to_owned())),
        house_number: Set(Some("45/2".to_owned())),
        ..user
    }
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let shop = shop::ActiveModel {
        promptpay_id: Set(Some("0812345678".to_owned())),
        ..testing::new_shop(seller)
    };
    let shop = shop.insert(&db).await.unwrap().id;
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
    let buyer = housed(testing::new_user(BUYER));
    let buyer = buyer.insert(&db).await.unwrap().id;
    let other_buyer = housed(testing::new_user(OTHER_BUYER));
    let other_buyer = other_buyer.insert(&db).await.unwrap().id;
    let category = testing::new_category("ไข่/นม").insert(&db).await.unwrap().id;
    let eggs = testing::new_product(shop, category, "ไข่ไก่เบอร์ 2", Some(8))
        .insert(&db)
        .await
        .unwrap()
        .id;
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        seller,
        shop,
        other_shop,
        buyer,
        other_buyer,
        category,
        eggs,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `now` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh. Returns the
    /// reply, and whether it is essential (FR-LINE-07).
    async fn handle_at(
        &self,
        user_id: i64,
        now: DateTimeUtc,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> (Vec<Message>, bool) {
        self.handle_on(&self.db, user_id, now, handle).await
    }

    /// As [`Setup::handle_at`], on `db`, which may be a pool of its own.
    async fn handle_on(
        &self,
        db: &DatabaseConnection,
        user_id: i64,
        now: DateTimeUtc,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> (Vec<Message>, bool) {
        let txn = db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, now)
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages,
            essential,
            ..
        } = turn.finish(messages);
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        (messages, essential)
    }

    /// User `user_id` taps "สั่งซื้อร้านนี้" of shop `shop_id` at `now`.
    async fn checks_out_at(&self, user_id: i64, shop_id: i64, now: DateTimeUtc) -> Vec<Message> {
        let postback = Postback::parse(&format!("checkout?shop={shop_id}")).unwrap();
        let config = &self.config;
        let handled = self
            .handle_at(user_id, now, async |turn| {
                tapped(turn, config, &postback).await
            })
            .await;
        handled.0
    }

    async fn checks_out(&self, user_id: i64, shop_id: i64) -> Vec<Message> {
        self.checks_out_at(user_id, shop_id, t0()).await
    }

    /// User `user_id` sends `input` at `now` to the flow they are in, checkout's or the
    /// address's, as routing hands it over.
    async fn sends_at(&self, user_id: i64, input: Input, now: DateTimeUtc) -> Vec<Message> {
        let config = &self.config;
        let mut answered = None;
        self.handle_at(user_id, now, async |turn| {
            let flow = turn.flow.as_ref().expect("in a flow");
            answered = if let Some(session) = flow.read::<CheckoutFlow>() {
                answer(turn, config, session, &input).await?
            } else if let Some(session) = flow.read::<AddressFlow>() {
                address::answer(turn, config, session, &input).await?
            } else {
                panic!("in flow {}", flow.flow());
            };
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered.expect("an answer")
    }

    async fn says_at(&self, user_id: i64, text: &str, now: DateTimeUtc) -> Vec<Message> {
        self.sends_at(user_id, Input::Text(text.to_owned()), now)
            .await
    }

    async fn says(&self, user_id: i64, text: &str) -> Vec<Message> {
        self.says_at(user_id, text, t0()).await
    }

    /// User `user_id` taps the confirm button sending `data` at `now`.
    async fn confirms_at(
        &self,
        user_id: i64,
        data: &str,
        now: DateTimeUtc,
    ) -> (Vec<Message>, bool) {
        let postback = Postback::parse(data).unwrap();
        self.handle_at(user_id, now, async |turn| {
            pending::tapped(turn, &postback).await
        })
        .await
    }

    async fn confirms(&self, user_id: i64, data: &str) -> (Vec<Message>, bool) {
        self.confirms_at(user_id, data, t0()).await
    }

    /// User `user_id` checks out [`Setup::shop`] at `now` as in Appendix A.2, delivered and
    /// paid in cash with no note, to its summary, and returns what its confirm button sends.
    async fn summary_at(&self, user_id: i64, now: DateTimeUtc) -> String {
        self.checks_out_at(user_id, self.shop, now).await;
        for answer in ["ให้ไปส่ง", "ใช่", "เงินสดตอนรับของ"]
        {
            self.says_at(user_id, answer, now).await;
        }
        confirm_of(&self.says_at(user_id, "ไม่มี", now).await)
    }

    async fn summary(&self, user_id: i64) -> String {
        self.summary_at(user_id, t0()).await
    }

    /// User `user_id` checks out [`Setup::shop`] at `now` as in Appendix A.2 and confirms.
    async fn places_at(&self, user_id: i64, now: DateTimeUtc) -> Vec<Message> {
        let confirm = self.summary_at(user_id, now).await;
        self.confirms_at(user_id, &confirm, now).await.0
    }

    /// A product of shop `shop_id` called `name`, at ฿110 a แผง, with `stock`.
    async fn product(&self, shop_id: i64, name: &str, stock: Option<i32>) -> i64 {
        let product = testing::new_product(shop_id, self.category, name, stock);
        product.insert(&self.db).await.unwrap().id
    }

    /// Puts `quantity` of product `id` in user `user_id`'s cart, added at `at`.
    async fn put_at(&self, user_id: i64, id: i64, quantity: i32, at: DateTimeUtc) {
        let line = cart_item::ActiveModel {
            user_id: Set(user_id),
            product_id: Set(id),
            quantity: Set(quantity),
            added_at: Set(at),
        };
        line.insert(&self.db).await.unwrap();
    }

    async fn put(&self, user_id: i64, id: i64, quantity: i32) {
        self.put_at(user_id, id, quantity, t0()).await;
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

    /// User `user_id`'s flow in progress, by name.
    async fn flow_of(&self, user_id: i64) -> Option<String> {
        let session = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap();
        session.map(|session| session.flow)
    }

    async fn orders(&self) -> Vec<order::Model> {
        order::Entity::find()
            .order_by_asc(order::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn items(&self, order_id: i64) -> Vec<order_item::Model> {
        order_item::Entity::find()
            .filter(order_item::Column::OrderId.eq(order_id))
            .order_by_asc(order_item::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn stock(&self, id: i64) -> Option<i32> {
        let product = product::Entity::find_by_id(id).one(&self.db).await.unwrap();
        product.expect("a product").stock
    }

    async fn update_product(&self, id: i64, column: product::Column, value: Expr) {
        product::Entity::update_many()
            .col_expr(column, value)
            .filter(product::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    async fn set_stock(&self, id: i64, stock: Option<i32>) {
        let stock = Expr::value(stock);
        self.update_product(id, product::Column::Stock, stock).await;
    }

    async fn set_price(&self, id: i64, baht: i64) {
        let price = Expr::value(Money::from_baht(baht));
        self.update_product(id, product::Column::Price, price).await;
    }

    /// Changes shop `id` with `change`.
    async fn update_shop(&self, id: i64, change: impl FnOnce(&mut shop::ActiveModel)) {
        let shop = shop::Entity::find_by_id(id).one(&self.db).await.unwrap();
        let mut shop = shop.unwrap().into_active_model();
        change(&mut shop);
        shop.update(&self.db).await.unwrap();
    }

    async fn set_user_status(&self, id: i64, status: user::Status) {
        user::Entity::update_many()
            .col_expr(user::Column::Status, Expr::value(status))
            .filter(user::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Puts everything back as [`setup`] made it, but the orders: the shop, the eggs, the
    /// sellers, and the buyer's cart, which holds 2 eggs.
    async fn reset(&self) {
        self.update_shop(self.shop, |shop| {
            shop.status = Set(shop::Status::Approved);
            shop.is_open = Set(true);
            shop.offers_pickup = Set(true);
            shop.offers_delivery = Set(true);
            shop.delivery_fee = Set(Money::from_baht(10));
            shop.minimum_order = Set(None);
            shop.promptpay_id = Set(Some("0812345678".to_owned()));
        })
        .await;
        self.set_stock(self.eggs, Some(8)).await;
        self.set_price(self.eggs, 110).await;
        let active = Expr::value(product::Status::Active);
        self.update_product(self.eggs, product::Column::Status, active)
            .await;
        self.set_user_status(self.seller, user::Status::Active)
            .await;
        cart_item::Entity::delete_many()
            .filter(cart_item::Column::UserId.eq(self.buyer))
            .exec(&self.db)
            .await
            .unwrap();
        self.put(self.buyer, self.eggs, 2).await;
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The text of each of `messages`.
fn texts_of(messages: &[Message]) -> Vec<&str> {
    messages.iter().map(text).collect()
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

/// What `action` sends: its postback data, or the text it sends as typed; `location` for
/// LINE's location picker.
fn sends(action: &Action) -> &str {
    match action {
        Action::Postback { data, .. } => data,
        Action::Message { text, .. } => text,
        Action::Location { .. } => "location",
        action => panic!("neither postback, text nor location: {action:?}"),
    }
}

/// What `message`'s Quick Reply buttons send.
fn quick_reply_data(message: &Message) -> Vec<&str> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| sends(&item.action))
        .collect()
}

/// `message`'s alt text and card.
fn card(message: &Message) -> (&str, &Bubble) {
    match &message.body {
        Body::Flex {
            alt_text,
            contents: FlexContainer::Bubble(bubble),
        } => (alt_text, bubble),
        body => panic!("not a card: {body:?}"),
    }
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

/// The label of each button at `card`'s foot, and what it sends.
fn buttons(card: &Bubble) -> Vec<(String, String)> {
    let Some(footer) = &card.footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => (
                button.action.label().as_str().to_owned(),
                sends(&button.action).to_owned(),
            ),
            component => panic!("not a button: {component:?}"),
        })
        .collect()
}

/// What the confirm button of the summary among `messages`, the last, sends.
fn confirm_of(messages: &[Message]) -> String {
    let (_, summary) = card(messages.last().expect("a summary"));
    let buttons = buttons(summary);
    assert_eq!(buttons[0].0, "ยืนยันสั่งซื้อ");
    buttons[0].1.clone()
}

/// The pending action a confirm button sending `data` confirms.
fn pending_id(data: &str) -> Uuid {
    Postback::parse(data).unwrap().uuid("pa").unwrap()
}

/// FR-CART-04 – FR-CART-08, FR-ORD-01, DR-03, Appendix A.2: a returning buyer is asked each
/// step with its choices, sees the summary, and confirms; the order is placed, numbered,
/// copied and keyed by the confirmation, its stock reserved and its lines out of the cart, and
/// the card says so, essential, with "ยกเลิกออเดอร์".
#[tokio::test]
async fn a_returning_buyer_checks_out_as_in_appendix_a2() {
    let s = setup().await;
    s.put(s.buyer, s.eggs, 2).await;

    let asked = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(texts_of(&asked), ["รับเองที่ร้าน หรือให้ร้านไปส่งคะ"]);
    assert_eq!(quick_replies(&asked[0]), ["รับเอง", "ให้ไปส่ง +฿10", "ยกเลิก"]);
    assert_eq!(quick_reply_data(&asked[0]), ["รับเอง", "ให้ไปส่ง", "ยกเลิก"]);
    assert_eq!(s.flow_of(s.buyer).await.as_deref(), Some("checkout"));

    let asked = s.says(s.buyer, "ให้ไปส่ง").await;
    assert_eq!(texts_of(&asked), ["ส่งที่ ซอย 3 บ้านเลขที่ 45/2 ใช่ไหมคะ"]);
    assert_eq!(
        quick_reply_data(&asked[0]),
        ["ใช่", "เปลี่ยนที่อยู่", "location", "ยกเลิก"]
    );
    let asked = s.says(s.buyer, "ใช่").await;
    assert_eq!(texts_of(&asked), ["ชำระเงินแบบไหนคะ"]);
    assert_eq!(
        quick_replies(&asked[0]),
        ["เงินสดตอนรับของ", "โอนพร้อมเพย์", "ยกเลิก"]
    );
    let asked = s.says(s.buyer, "เงินสดตอนรับของ").await;
    assert_eq!(texts_of(&asked), ["มีอะไรฝากถึงร้านไหมคะ พิมพ์ได้เลย"]);
    assert_eq!(quick_replies(&asked[0]), ["ไม่มี", "ยกเลิก"]);

    let summary = s.says(s.buyer, "ไม่มี").await;
    assert_eq!(summary.len(), 1);
    let (alt_text, shown) = card(&summary[0]);
    assert_eq!(alt_text, "สรุปคำสั่งซื้อ รวม ฿230");
    assert_eq!(
        texts(shown),
        [
            "สรุปคำสั่งซื้อ",
            "ร้านป้าแดง",
            "ไข่ไก่เบอร์ 2",
            "2 แผง × ฿110",
            "฿220",
            "ค่าสินค้า",
            "฿220",
            "ค่าส่ง",
            "฿10",
            "รวม",
            "฿230",
            "รับของ",
            "ส่งที่ ซอย 3 บ้านเลขที่ 45/2",
            "ชำระเงิน",
            "เงินสดตอนรับของ",
        ]
    );
    let confirm = confirm_of(&summary);
    assert_eq!(
        buttons(shown),
        [
            ("ยืนยันสั่งซื้อ".to_owned(), confirm.clone()),
            ("แก้ไข".to_owned(), format!("checkout?shop={}", s.shop)),
            ("ยกเลิก".to_owned(), "ยกเลิก".to_owned()),
        ]
    );
    // The summary ends the flow; its confirmation lasts longer
    assert_eq!(s.flow_of(s.buyer).await, None);
    assert!(s.orders().await.is_empty());

    let (placed, essential) = s.confirms_at(s.buyer, &confirm, t0() + minutes(30)).await;
    assert!(essential, "pushed should its reply token fail (FR-LINE-07)");
    let orders = s.orders().await;
    assert_eq!(orders.len(), 1);
    let order = &orders[0];
    let at = t0() + minutes(30);
    assert_eq!(
        order,
        &order::Model {
            id: order.id,
            order_number: "IM-20261025-0001".to_owned(),
            checkout_key: pending_id(&confirm),
            buyer_id: s.buyer,
            shop_id: s.shop,
            status: order::Status::Pending,
            fulfillment: Fulfillment::Delivery,
            delivery_zone: Some("ซอย 3".to_owned()),
            delivery_house_number: Some("45/2".to_owned()),
            delivery_landmark_note: None,
            delivery_latitude: None,
            delivery_longitude: None,
            note: None,
            subtotal: Money::from_baht(220),
            delivery_fee: Money::from_baht(10),
            total: Money::from_baht(230),
            payment_method: PaymentMethod::Cash,
            payment_status: order::PaymentStatus::Unpaid,
            created_at: at,
            updated_at: at,
            accepted_at: None,
            ready_at: None,
            delivering_at: None,
            completed_at: None,
            rejected_at: None,
            cancelled_at: None,
            expired_at: None,
            cancel_reason: None,
            cancel_requested_at: None,
        }
    );
    let items: Vec<_> = s
        .items(order.id)
        .await
        .into_iter()
        .map(|item| {
            let price = (item.unit_price, item.line_total);
            (item.product_id, item.name, item.unit, price, item.quantity)
        })
        .collect();
    let price = (Money::from_baht(110), Money::from_baht(220));
    assert_eq!(
        items,
        [(s.eggs, "ไข่ไก่เบอร์ 2".to_owned(), "แผง".to_owned(), price, 2)]
    );
    let events = order_event::Entity::find().all(&s.db).await.unwrap();
    let events: Vec<_> = events
        .into_iter()
        .map(|event| {
            let actor = (event.actor_type, event.actor_id);
            (event.order_id, event.from_status, event.to_status, actor)
        })
        .collect();
    let actor = (order_event::ActorType::Buyer, Some(s.buyer));
    assert_eq!(events, [(order.id, None, order::Status::Pending, actor)]);
    assert_eq!(s.stock(s.eggs).await, Some(6));
    assert_eq!(s.cart_of(s.buyer).await, []);

    assert_eq!(placed.len(), 1);
    let (alt_text, shown) = card(&placed[0]);
    assert_eq!(alt_text, "สั่งซื้อเรียบร้อยค่ะ เลขที่ IM-20261025-0001 · รอร้านยืนยัน");
    assert_eq!(
        texts(shown)[..6],
        [
            "สั่งซื้อเรียบร้อยค่ะ",
            "เลขที่",
            "IM-20261025-0001",
            "สถานะ",
            "รอร้านยืนยัน",
            "ร้านป้าแดง",
        ]
    );
    assert!(texts(shown).contains(&"฿230".to_owned()));
    assert_eq!(
        buttons(shown),
        [(
            "ยกเลิกออเดอร์".to_owned(),
            format!("order.cancel?order={}", order.id)
        )]
    );
    assert_eq!(quick_replies(&placed[0]), ["เลือกซื้อต่อ", "เมนู"]);

    // DR-03: the order keeps the product as it was
    s.set_price(s.eggs, 150).await;
    let renamed = Expr::value("ไข่ไก่เบอร์ 0");
    s.update_product(s.eggs, product::Column::Name, renamed)
        .await;
    let item = &s.items(order.id).await[0];
    assert_eq!(
        (item.name.as_str(), item.unit_price),
        ("ไข่ไก่เบอร์ 2", Money::from_baht(110))
    );
}

/// FR-CART-04: a step with one choice is skipped: pickup only skips the address too, and a
/// shop without a PromptPay ID, or `payment.methods` allowing cash only, skips the payment
/// method. A picked-up order has no delivery fee and keeps no address.
#[tokio::test]
async fn steps_with_one_choice_are_skipped() {
    let mut s = setup().await;
    s.put(s.buyer, s.eggs, 2).await;
    s.update_shop(s.shop, |shop| {
        shop.offers_delivery = Set(false);
        shop.promptpay_id = Set(None);
    })
    .await;

    let asked = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(texts_of(&asked), ["มีอะไรฝากถึงร้านไหมคะ พิมพ์ได้เลย"]);
    let summary = s.says(s.buyer, "ไม่มี").await;
    let (alt_text, shown) = card(&summary[0]);
    assert_eq!(alt_text, "สรุปคำสั่งซื้อ รวม ฿220");
    let shown = texts(shown);
    assert!(!shown.contains(&"ค่าส่ง".to_owned()), "{shown:?}");
    assert!(shown.contains(&"รับเองที่ร้าน".to_owned()), "{shown:?}");
    s.confirms(s.buyer, &confirm_of(&summary)).await;
    let order = s.orders().await.remove(0);
    assert_eq!(
        (order.fulfillment, order.delivery_fee, order.total),
        (Fulfillment::Pickup, Money::ZERO, Money::from_baht(220))
    );
    assert_eq!(
        (order.delivery_zone, order.delivery_house_number),
        (None, None)
    );
    assert_eq!(order.payment_method, PaymentMethod::Cash);

    // Delivered only, and cash only: the address is confirmed first, then the note
    s.update_shop(s.shop, |shop| {
        shop.offers_pickup = Set(false);
        shop.offers_delivery = Set(true);
        shop.promptpay_id = Set(Some("0812345678".to_owned()));
    })
    .await;
    s.config.payment.methods = vec![config::PaymentMethod::Cash];
    s.put(s.buyer, s.eggs, 1).await;
    let asked = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(texts_of(&asked), ["ส่งที่ ซอย 3 บ้านเลขที่ 45/2 ใช่ไหมคะ"]);
    let asked = s.says(s.buyer, "ใช่").await;
    assert_eq!(texts_of(&asked), ["มีอะไรฝากถึงร้านไหมคะ พิมพ์ได้เลย"]);

    // A free delivery's button says only that
    s.update_shop(s.shop, |shop| {
        shop.offers_pickup = Set(true);
        shop.delivery_fee = Set(Money::ZERO);
    })
    .await;
    let asked = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(quick_replies(&asked[0]), ["รับเอง", "ให้ไปส่ง", "ยกเลิก"]);
}

/// FR-ONB-04: a buyer without an address gives it first, then checkout goes on from its first
/// step.
#[tokio::test]
async fn the_address_is_asked_for_first() {
    let s = setup().await;
    let t = &s.templates;
    let newcomer = testing::new_user(NEWCOMER).insert(&s.db).await.unwrap().id;
    s.put(newcomer, s.eggs, 1).await;

    s.checks_out(newcomer, s.shop).await;
    assert_eq!(s.flow_of(newcomer).await.as_deref(), Some("address"));
    // Zone and house number, then no landmark, phone or location
    for answer in ["ซอย 5", "12", "ข้าม", "ข้าม"] {
        s.says(newcomer, answer).await;
    }
    let resumed = s.says(newcomer, "ข้าม").await;
    assert_eq!(
        texts_of(&resumed),
        [
            text(&t.address_saved("ซอย 5 บ้านเลขที่ 12")),
            "รับเองที่ร้าน หรือให้ร้านไปส่งคะ",
        ]
    );
    assert_eq!(s.flow_of(newcomer).await.as_deref(), Some("checkout"));
    let asked = s.says(newcomer, "ให้ไปส่ง").await;
    assert_eq!(texts_of(&asked), ["ส่งที่ ซอย 5 บ้านเลขที่ 12 ใช่ไหมคะ"]);
}

/// FR-CART-04: the address to deliver to is changed, which then goes on to the payment method,
/// or a location is shared, which is kept as the buyer's; the order keeps either.
#[tokio::test]
async fn the_address_is_changed_or_a_location_shared() {
    let s = setup().await;
    let t = &s.templates;
    s.put(s.buyer, s.eggs, 1).await;
    s.checks_out(s.buyer, s.shop).await;
    s.says(s.buyer, "ให้ไปส่ง").await;

    s.says(s.buyer, "เปลี่ยนที่อยู่").await;
    assert_eq!(s.flow_of(s.buyer).await.as_deref(), Some("address"));
    s.says(s.buyer, "ซอย 7").await;
    s.says(s.buyer, "8/1").await;
    let resumed = s.says(s.buyer, "หน้าวัด").await;
    assert_eq!(
        texts_of(&resumed),
        [
            text(&t.address_saved("ซอย 7 บ้านเลขที่ 8/1 (หน้าวัด)")),
            "ชำระเงินแบบไหนคะ",
        ]
    );
    s.says(s.buyer, "เงินสดตอนรับของ").await;
    let summary = s.says(s.buyer, "ไม่มี").await;
    let shown = texts(card(&summary[0]).1);
    assert!(shown.contains(&"ส่งที่ ซอย 7 บ้านเลขที่ 8/1 (หน้าวัด)".to_owned()));
    s.confirms(s.buyer, &confirm_of(&summary)).await;
    let order = s.orders().await.remove(0);
    assert_eq!(
        (
            order.delivery_zone.as_deref(),
            order.delivery_house_number.as_deref(),
            order.delivery_landmark_note.as_deref(),
            order.delivery_latitude,
        ),
        (Some("ซอย 7"), Some("8/1"), Some("หน้าวัด"), None)
    );

    s.put(s.buyer, s.eggs, 1).await;
    s.checks_out(s.buyer, s.shop).await;
    s.says(s.buyer, "ให้ไปส่ง").await;
    let shared = Input::Location(Location {
        latitude: 13.7563,
        longitude: 100.5018,
        address: Some("ถนนสุขุมวิท".to_owned()),
    });
    let asked = s.sends_at(s.buyer, shared, t0()).await;
    assert_eq!(texts_of(&asked), ["บันทึกตำแหน่งแล้วค่ะ", "ชำระเงินแบบไหนคะ"]);
    let buyer = user::Entity::find_by_id(s.buyer).one(&s.db).await.unwrap();
    let buyer = buyer.unwrap();
    assert_eq!(
        (buyer.latitude, buyer.longitude),
        (Some(13.7563), Some(100.5018))
    );
    s.says(s.buyer, "เงินสดตอนรับของ").await;
    let summary = s.says(s.buyer, "ไม่มี").await;
    s.confirms(s.buyer, &confirm_of(&summary)).await;
    let order = s.orders().await.remove(1);
    assert_eq!(
        (order.delivery_latitude, order.delivery_longitude),
        (Some(13.7563), Some(100.5018))
    );

    // Next time the address shown says so
    s.put(s.buyer, s.eggs, 1).await;
    s.checks_out(s.buyer, s.shop).await;
    let asked = s.says(s.buyer, "ให้ไปส่ง").await;
    assert_eq!(
        texts_of(&asked),
        ["ส่งที่ ซอย 7 บ้านเลขที่ 8/1 (หน้าวัด) ตามตำแหน่งที่แชร์ไว้ ใช่ไหมคะ"]
    );
}

/// FR-CART-04: the note goes to the shop with the order; a longer one than 200 characters is
/// refused, and an answer that is no choice is asked again, as is a choice the shop no longer
/// takes.
#[tokio::test]
async fn only_what_a_step_takes_is_taken() {
    let s = setup().await;
    let t = &s.templates;
    s.put(s.buyer, s.eggs, 2).await;
    s.checks_out(s.buyer, s.shop).await;

    let again = s.says(s.buyer, "ส่งด่วน").await;
    assert_eq!(texts_of(&again), ["รับเองที่ร้าน หรือให้ร้านไปส่งคะ"]);
    s.says(s.buyer, "ให้ไปส่ง").await;
    let again = s.says(s.buyer, "ไม่ใช่").await;
    assert_eq!(texts_of(&again), ["ส่งที่ ซอย 3 บ้านเลขที่ 45/2 ใช่ไหมคะ"]);
    s.says(s.buyer, "ใช่").await;
    // The shop stopped taking PromptPay meanwhile
    s.update_shop(s.shop, |shop| shop.promptpay_id = Set(None))
        .await;
    let again = s.says(s.buyer, "โอนพร้อมเพย์").await;
    assert_eq!(texts_of(&again), ["ชำระเงินแบบไหนคะ"]);
    assert_eq!(quick_replies(&again[0]), ["เงินสดตอนรับของ", "ยกเลิก"]);
    s.says(s.buyer, "เงินสดตอนรับของ").await;

    let refused = s.says(s.buyer, &"ก".repeat(201)).await;
    assert_eq!(texts_of(&refused), [text(&t.text_too_long(200))]);
    assert_eq!(quick_replies(&refused[0]), ["ไม่มี", "ยกเลิก"]);
    let summary = s.says(s.buyer, "  วางไว้หน้าบ้านได้เลย  ").await;
    let shown = texts(card(&summary[0]).1);
    let note = "วางไว้หน้าบ้านได้เลย".to_owned();
    assert!(shown.ends_with(&["ฝากถึงร้าน".to_owned(), note.clone()]));
    s.confirms(s.buyer, &confirm_of(&summary)).await;
    assert_eq!(s.orders().await[0].note, Some(note));
}

/// FR-CART-04: a shop that no longer offers what the buyer chose has them choose again.
#[tokio::test]
async fn choices_the_shop_no_longer_offers_are_made_again() {
    let s = setup().await;
    s.put(s.buyer, s.eggs, 2).await;
    s.checks_out(s.buyer, s.shop).await;
    s.says(s.buyer, "ให้ไปส่ง").await;
    s.update_shop(s.shop, |shop| shop.offers_delivery = Set(false))
        .await;

    // Picked up then, the only way left, so on to the payment method
    let asked = s.says(s.buyer, "ใช่").await;
    assert_eq!(
        texts_of(&asked),
        [
            "ขอโทษค่ะ ร้านเปลี่ยนวิธีรับของหรือการชำระเงินแล้ว เลือกใหม่อีกครั้งนะคะ",
            "ชำระเงินแบบไหนคะ",
        ]
    );
    s.says(s.buyer, "เงินสดตอนรับของ").await;
    let summary = s.says(s.buyer, "ไม่มี").await;
    let shown = texts(card(&summary[0]).1);
    assert!(shown.contains(&"รับเองที่ร้าน".to_owned()), "{shown:?}");
}

/// FR-ORD-01: order numbers count up through the day in Bangkok, then start again.
#[tokio::test]
async fn orders_are_numbered_by_the_day_in_bangkok() {
    let s = setup().await;
    // 23:59:59 in Bangkok, then midnight
    let late = Utc.with_ymd_and_hms(2026, 10, 25, 16, 59, 59).unwrap();
    let midnight = late + TimeDelta::seconds(1);
    for (user_id, at) in [
        (s.buyer, t0()),
        (s.other_buyer, t0()),
        (s.buyer, late),
        (s.buyer, midnight),
    ] {
        s.put(user_id, s.eggs, 1).await;
        s.places_at(user_id, at).await;
    }
    let numbers: Vec<_> = s
        .orders()
        .await
        .into_iter()
        .map(|order| order.order_number)
        .collect();
    assert_eq!(
        numbers,
        [
            "IM-20261025-0001",
            "IM-20261025-0002",
            "IM-20261025-0003",
            "IM-20261026-0001",
        ]
    );
}

/// FR-UI-13, NFR-REL-02: a repeated tap of the confirmation places no second order and shows
/// the one placed.
#[tokio::test]
async fn a_repeated_tap_places_one_order() {
    let s = setup().await;
    s.put(s.buyer, s.eggs, 2).await;
    let confirm = s.summary(s.buyer).await;
    s.confirms(s.buyer, &confirm).await;

    let (again, essential) = s.confirms_at(s.buyer, &confirm, t0() + minutes(1)).await;
    assert!(!essential);
    let (alt_text, shown) = card(&again[0]);
    assert_eq!(alt_text, "สั่งซื้อนี้ไว้แล้วค่ะ เลขที่ IM-20261025-0001 · รอร้านยืนยัน");
    let order = s.orders().await.remove(0);
    assert_eq!(
        buttons(shown),
        [(
            "ยกเลิกออเดอร์".to_owned(),
            format!("order.cancel?order={}", order.id)
        )]
    );
    assert_eq!(s.orders().await.len(), 1);
    assert_eq!(s.stock(s.eggs).await, Some(6));
}

/// AC-12, FR-CART-05: a price changed since the summary places no order, and the summary is
/// shown again at the new price, which then places it.
#[tokio::test]
async fn a_price_changed_since_the_summary_is_shown_first() {
    let s = setup().await;
    s.put(s.buyer, s.eggs, 2).await;
    let confirm = s.summary(s.buyer).await;
    s.set_price(s.eggs, 120).await;

    let (changed, essential) = s.confirms(s.buyer, &confirm).await;
    assert!(!essential);
    assert!(s.orders().await.is_empty());
    assert_eq!(
        (s.stock(s.eggs).await, s.cart_of(s.buyer).await),
        (Some(8), vec![(s.eggs, 2)])
    );
    assert_eq!(changed.len(), 2);
    assert_eq!(
        text(&changed[0]),
        "ขอโทษค่ะ ข้อมูลเปลี่ยนไประหว่างสั่งซื้อ จึงยังไม่ได้สั่งซื้อ\n\
         • \"ไข่ไก่เบอร์ 2\" ราคาเปลี่ยนจาก ฿110 / แผง เป็น ฿120 / แผง\n\
         ตรวจสรุปใหม่ด้านล่าง แล้วกด \"ยืนยันสั่งซื้อ\" อีกครั้งนะคะ"
    );
    let (alt_text, shown) = card(&changed[1]);
    assert_eq!(alt_text, "สรุปคำสั่งซื้อ รวม ฿250");
    let shown = texts(shown);
    assert!(shown.contains(&"2 แผง × ฿120".to_owned()), "{shown:?}");

    let again = confirm_of(&changed);
    assert_ne!(again, confirm);
    s.confirms(s.buyer, &again).await;
    let order = s.orders().await.remove(0);
    assert_eq!(order.total, Money::from_baht(250));
    assert_eq!(s.items(order.id).await[0].unit_price, Money::from_baht(120));
}

/// FR-PROD-11, FR-CART-05, DR-03: a pre-order's ready date changed since the summary places no
/// order, and the summary is shown again with the new one, which then places it, the order's
/// line and card keeping it.
#[tokio::test]
async fn a_pre_order_is_placed_with_the_ready_date_shown() {
    let s = setup().await;
    let ready_on = |day: u32| NaiveDate::from_ymd_opt(2026, 11, day);
    let ready =
        |day: u32| s.update_product(s.eggs, product::Column::ReadyOn, Expr::value(ready_on(day)));
    ready(5).await;
    s.put(s.buyer, s.eggs, 2).await;
    let confirm = s.summary(s.buyer).await;
    ready(7).await;

    let (changed, _) = s.confirms(s.buyer, &confirm).await;
    assert!(s.orders().await.is_empty());
    assert_eq!(
        text(&changed[0]),
        "ขอโทษค่ะ ข้อมูลเปลี่ยนไประหว่างสั่งซื้อ จึงยังไม่ได้สั่งซื้อ\n\
         • \"ไข่ไก่เบอร์ 2\" เปลี่ยนวันพร้อมเป็น 7 พ.ย. 2569\n\
         ตรวจสรุปใหม่ด้านล่าง แล้วกด \"ยืนยันสั่งซื้อ\" อีกครั้งนะคะ"
    );
    let ready_line = "สั่งล่วงหน้า · พร้อม 7 พ.ย. 2569".to_owned();
    let (_, summary) = card(&changed[1]);
    let shown = texts(summary);
    let at = shown.iter().position(|text| *text == "฿220").unwrap();
    assert_eq!(shown[at + 1], ready_line);

    let (placed, _) = s.confirms(s.buyer, &confirm_of(&changed)).await;
    let order = s.orders().await.remove(0);
    assert_eq!(s.items(order.id).await[0].ready_on, ready_on(7));
    let (_, card) = card(&placed[0]);
    assert!(texts(card).contains(&ready_line), "{:?}", texts(card));
    // The order keeps it, whatever becomes of the product's
    ready(9).await;
    assert_eq!(s.items(order.id).await[0].ready_on, ready_on(7));
}

/// A summary shown before ready dates were kept with it, as the release before them stored it,
/// still places its order (FR-UI-13).
#[tokio::test]
async fn a_summary_from_before_ready_dates_still_places_its_order() {
    let s = setup().await;
    s.put(s.buyer, s.eggs, 2).await;
    let confirm = s.summary(s.buyer).await;
    let id = pending_id(&confirm);
    let stored = pending_action::Entity::find_by_id(id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    let mut payload = stored.payload.clone();
    for line in payload["lines"].as_array_mut().unwrap() {
        assert!(line.as_object_mut().unwrap().remove("ready_on").is_some());
    }
    pending_action::ActiveModel {
        payload: Set(payload),
        ..stored.into_active_model()
    }
    .update(&s.db)
    .await
    .unwrap();

    s.confirms(s.buyer, &confirm).await;
    assert_eq!(s.orders().await.len(), 1);
}

/// FR-CART-05, FR-UI-13: anything changed since the summary places no order, and says what
/// changed, then what the buyer can do now.
#[tokio::test]
async fn what_changed_since_the_summary_places_nothing() {
    let s = setup().await;
    let t = &s.templates;
    let sorry = "ขอโทษค่ะ สินค้าหมดแล้ว จึงยังไม่ได้สั่งซื้อ";
    let carry_on = ["ดูตะกร้า", "เลือกซื้อต่อ", "เมนู"];

    // Each change, and the reply's texts, or the first one's when a summary follows
    s.reset().await;
    let confirm = s.summary(s.buyer).await;
    s.set_stock(s.eggs, Some(0)).await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert_eq!(
        texts_of(&said),
        [format!(
            "{sorry}\n• \"ไข่ไก่เบอร์ 2\" หมดแล้ว\nดูตะกร้าหรือเลือกซื้อต่อได้เลยค่ะ"
        )]
    );
    assert_eq!(quick_replies(&said[0]), carry_on);

    s.reset().await;
    let confirm = s.summary(s.buyer).await;
    s.set_stock(s.eggs, Some(1)).await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert_eq!(
        texts_of(&said),
        [
            "ขอโทษค่ะ ข้อมูลเปลี่ยนไประหว่างสั่งซื้อ จึงยังไม่ได้สั่งซื้อ\n• \"ไข่ไก่เบอร์ 2\" เหลือแค่ 1 แผง\nดูตะกร้าหรือเลือกซื้อต่อได้เลยค่ะ"
        ]
    );

    s.reset().await;
    let confirm = s.summary(s.buyer).await;
    let hidden = Expr::value(product::Status::Hidden);
    s.update_product(s.eggs, product::Column::Status, hidden)
        .await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert!(text(&said[0]).contains("• \"ไข่ไก่เบอร์ 2\" ตอนนี้ไม่มีขาย"));

    // The shop closed, or its seller suspended
    for suspended in [false, true] {
        s.reset().await;
        let confirm = s.summary(s.buyer).await;
        if suspended {
            s.set_user_status(s.seller, user::Status::Suspended).await;
        } else {
            s.update_shop(s.shop, |shop| shop.is_open = Set(false))
                .await;
        }
        let (said, _) = s.confirms(s.buyer, &confirm).await;
        assert!(text(&said[0]).contains("• ร้านป้าแดงไม่รับออเดอร์ตอนนี้"));
    }

    // A changed delivery fee: the summary again
    s.reset().await;
    let confirm = s.summary(s.buyer).await;
    let fee = Money::from_baht(20);
    s.update_shop(s.shop, |shop| shop.delivery_fee = Set(fee))
        .await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert!(text(&said[0]).contains("• ค่าส่งเปลี่ยนจาก ฿10 เป็น ฿20"));
    assert_eq!(card(&said[1]).0, "สรุปคำสั่งซื้อ รวม ฿240");

    // A minimum order raised above the subtotal
    s.reset().await;
    let confirm = s.summary(s.buyer).await;
    let minimum = Money::from_baht(500);
    s.update_shop(s.shop, |shop| shop.minimum_order = Set(Some(minimum)))
        .await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    let below = t.below_minimum_order("ร้านป้าแดง", minimum, Money::from_baht(220));
    assert_eq!(texts_of(&said), [text(&below)]);

    // No longer delivered, or no longer paid by PromptPay: chosen again
    s.reset().await;
    let confirm = s.summary(s.buyer).await;
    s.update_shop(s.shop, |shop| shop.offers_delivery = Set(false))
        .await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert_eq!(texts_of(&said), [text(&t.checkout_options_changed())]);
    let again = [
        format!("checkout?shop={}", s.shop),
        "cart".to_owned(),
        "เมนู".to_owned(),
    ];
    assert_eq!(quick_reply_data(&said[0]), again);

    s.reset().await;
    s.checks_out(s.buyer, s.shop).await;
    for answer in ["ให้ไปส่ง", "ใช่", "โอนพร้อมเพย์"]
    {
        s.says(s.buyer, answer).await;
    }
    let confirm = confirm_of(&s.says(s.buyer, "ไม่มี").await);
    s.update_shop(s.shop, |shop| shop.promptpay_id = Set(None))
        .await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert_eq!(texts_of(&said), [text(&t.checkout_options_changed())]);

    // None of it placed an order or took any stock
    assert!(s.orders().await.is_empty());
    assert_eq!(s.cart_of(s.buyer).await, [(s.eggs, 2)]);
}

/// FR-UI-04, FR-UI-13: a summary cancelled, or confirmed too late, places nothing; while the
/// shop is known, "สั่งซื้อร้านนี้" checks it out again.
#[tokio::test]
async fn a_cancelled_or_expired_summary_places_nothing() {
    let s = setup().await;
    let t = &s.templates;
    s.put(s.buyer, s.eggs, 2).await;
    let confirm = s.summary(s.buyer).await;
    // "ยกเลิก", as typed or tapped
    s.handle_at(s.buyer, t0(), async |turn| flow::cancel(turn).await)
        .await;
    let (said, _) = s.confirms(s.buyer, &confirm).await;
    assert_eq!(texts_of(&said), [text(&t.checkout_gone())]);
    assert_eq!(quick_reply_data(&said[0]), ["cart", "เมนู"]);

    let confirm = s.summary(s.buyer).await;
    let late = t0() + minutes(61);
    let (said, _) = s.confirms_at(s.buyer, &confirm, late).await;
    assert_eq!(texts_of(&said), [text(&t.checkout_gone())]);
    assert_eq!(
        quick_reply_data(&said[0]),
        [format!("checkout?shop={}", s.shop).as_str(), "cart", "เมนู"]
    );
    assert!(s.orders().await.is_empty());
    assert_eq!(s.stock(s.eggs).await, Some(8));
}

/// FR-CART-03, FR-CART-05, FR-CART-07, FR-ADM-04: what can't be checked out is refused before
/// any step: nothing of the shop that can be ordered, less than its minimum order, the buyer's
/// own shop, more lines than an order holds, no payment method the shop takes, and a suspended
/// buyer.
#[tokio::test]
async fn checkouts_that_cannot_go_on_are_refused() {
    let mut s = setup().await;
    let t = &s.templates;
    let carry_on = ["ดูตะกร้า", "เลือกซื้อต่อ", "เมนู"];

    let refused = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(texts_of(&refused), [text(&t.nothing_to_order("ร้านป้าแดง"))]);
    assert_eq!(quick_replies(&refused[0]), carry_on);
    s.put(s.buyer, s.eggs, 1).await;
    s.set_stock(s.eggs, Some(0)).await;
    let refused = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(texts_of(&refused), [text(&t.nothing_to_order("ร้านป้าแดง"))]);
    s.set_stock(s.eggs, Some(8)).await;

    let minimum = Money::from_baht(500);
    s.update_shop(s.shop, |shop| shop.minimum_order = Set(Some(minimum)))
        .await;
    let refused = s.checks_out(s.buyer, s.shop).await;
    let below = t.below_minimum_order("ร้านป้าแดง", minimum, Money::from_baht(110));
    assert_eq!(texts_of(&refused), [text(&below)]);
    s.update_shop(s.shop, |shop| shop.minimum_order = Set(None))
        .await;

    s.put(s.seller, s.eggs, 1).await;
    let refused = s.checks_out(s.seller, s.shop).await;
    assert_eq!(texts_of(&refused), [text(&t.own_shop_order())]);

    for i in 0..cart::MAX_ITEMS {
        let id = s.product(s.shop, &format!("ผัก {i}"), None).await;
        s.put(s.buyer, id, 1).await;
    }
    let refused = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(texts_of(&refused), [text(&t.too_many_to_order(20))]);
    cart_item::Entity::delete_many()
        .filter(cart_item::Column::ProductId.ne(s.eggs))
        .exec(&s.db)
        .await
        .unwrap();

    s.config.payment.methods = vec![config::PaymentMethod::PromptPay];
    s.update_shop(s.shop, |shop| shop.promptpay_id = Set(None))
        .await;
    let refused = s.checks_out(s.buyer, s.shop).await;
    let t = &s.templates;
    assert_eq!(texts_of(&refused), [text(&t.no_payment_method("ร้านป้าแดง"))]);

    s.set_user_status(s.buyer, user::Status::Suspended).await;
    let refused = s.checks_out(s.buyer, s.shop).await;
    assert_eq!(
        texts_of(&refused),
        [text(&t.refused(Denied::Suspended, None))]
    );

    // None started checking out
    for user_id in [s.buyer, s.seller] {
        assert_eq!(s.flow_of(user_id).await, None);
    }
}

/// FR-CART-04: placing an order takes only its shop's lines out of the cart, and the stock of
/// the products that have one.
#[tokio::test]
async fn an_order_takes_its_shops_lines_and_their_stock() {
    let s = setup().await;
    let greens = s.product(s.shop, "ผักบุ้ง", None).await;
    let melon = s.product(s.other_shop, "แตงโม", Some(5)).await;
    s.put_at(s.buyer, s.eggs, 3, t0()).await;
    s.put_at(s.buyer, greens, 2, t0() + minutes(1)).await;
    s.put_at(s.buyer, melon, 1, t0() + minutes(2)).await;

    s.places_at(s.buyer, t0()).await;
    let order = s.orders().await.remove(0);
    assert_eq!(
        (order.subtotal, order.total),
        (Money::from_baht(550), Money::from_baht(560))
    );
    let items: Vec<_> = s
        .items(order.id)
        .await
        .into_iter()
        .map(|item| (item.product_id, item.quantity, item.line_total))
        .collect();
    assert_eq!(
        items,
        [
            (s.eggs, 3, Money::from_baht(330)),
            (greens, 2, Money::from_baht(220)),
        ]
    );
    assert_eq!(
        (
            s.stock(s.eggs).await,
            s.stock(greens).await,
            s.stock(melon).await
        ),
        (Some(5), None, Some(5))
    );
    assert_eq!(s.cart_of(s.buyer).await, [(melon, 1)]);
}

/// AC-03, FR-CART-06, DR-07, NFR-MNT-07: two buyers confirm the last one at the same moment, in
/// two transactions at once: exactly one order is placed, the other buyer is told
/// "สินค้าหมดแล้ว", and none is left.
#[tokio::test]
async fn the_last_one_goes_to_exactly_one_of_two_buyers() {
    let s = setup().await;
    s.set_stock(s.eggs, Some(1)).await;
    s.put(s.buyer, s.eggs, 1).await;
    s.put(s.other_buyer, s.eggs, 1).await;
    let mine = s.summary(s.buyer).await;
    let theirs = s.summary(s.other_buyer).await;

    let (one, other) = tokio::join!(
        s.confirms(s.buyer, &mine),
        s.confirms(s.other_buyer, &theirs)
    );
    let orders = s.orders().await;
    assert_eq!(orders.len(), 1);
    let (won, lost) = if orders[0].buyer_id == s.buyer {
        (one, other)
    } else {
        (other, one)
    };
    assert_eq!(
        card(&won.0[0]).0,
        "สั่งซื้อเรียบร้อยค่ะ เลขที่ IM-20261025-0001 · รอร้านยืนยัน"
    );
    assert_eq!(lost.0.len(), 1);
    assert!(
        text(&lost.0[0]).starts_with("ขอโทษค่ะ สินค้าหมดแล้ว"),
        "{}",
        text(&lost.0[0])
    );
    assert!(!lost.1);
    assert_eq!(s.stock(s.eggs).await, Some(0));
}

/// AC-03, DR-07, NFR-MNT-07: six buyers confirm 2 each of the last 5 at the same moment, each
/// on connections of their own, as the worker's events are: 2 orders are placed, and the 1 left
/// is neither sold nor lost.
#[tokio::test]
async fn buyers_racing_for_the_last_units_never_oversell() {
    let s = setup().await;
    s.set_stock(s.eggs, Some(5)).await;
    let mut buyers = vec![s.buyer, s.other_buyer];
    for line_user_id in ["U-racer-1", "U-racer-2", "U-racer-3", "U-racer-4"] {
        let buyer = housed(testing::new_user(line_user_id));
        buyers.push(buyer.insert(&s.db).await.unwrap().id);
    }
    let mut ready = Vec::new();
    for buyer in buyers {
        s.put(buyer, s.eggs, 2).await;
        let confirm = s.summary(buyer).await;
        let db = orm(testing::pool(&s._container).await);
        ready.push((buyer, confirm, db));
    }
    let confirm = async |i: usize| {
        let (buyer, data, db) = &ready[i];
        let postback = Postback::parse(data).unwrap();
        s.handle_on(db, *buyer, t0(), async |turn| {
            pending::tapped(turn, &postback).await
        })
        .await
    };

    let placed = tokio::join!(
        confirm(0),
        confirm(1),
        confirm(2),
        confirm(3),
        confirm(4),
        confirm(5),
    );
    let placed = [placed.0, placed.1, placed.2, placed.3, placed.4, placed.5]
        .iter()
        .filter(|(_, essential)| *essential)
        .count();

    assert_eq!(placed, 2);
    assert_eq!(s.orders().await.len(), 2);
    assert_eq!(s.stock(s.eggs).await, Some(1));
}

/// FR-CART-06, DR-07: checkouts of the same last products, put in their carts in opposite
/// orders, wait for one another rather than deadlock, and only one places its order, every
/// time.
#[tokio::test]
async fn checkouts_of_the_same_products_wait_for_one_another() {
    let s = setup().await;
    let milk = s.product(s.shop, "นมสด", Some(1)).await;
    for round in 0..5 {
        s.set_stock(s.eggs, Some(1)).await;
        s.set_stock(milk, Some(1)).await;
        cart_item::Entity::delete_many().exec(&s.db).await.unwrap();
        s.put_at(s.buyer, s.eggs, 1, t0()).await;
        s.put_at(s.buyer, milk, 1, t0() + minutes(1)).await;
        s.put_at(s.other_buyer, milk, 1, t0()).await;
        s.put_at(s.other_buyer, s.eggs, 1, t0() + minutes(1)).await;
        let mine = s.summary(s.buyer).await;
        let theirs = s.summary(s.other_buyer).await;

        let (one, other) = tokio::join!(
            s.confirms(s.buyer, &mine),
            s.confirms(s.other_buyer, &theirs)
        );
        let placed = [&one, &other]
            .iter()
            .filter(|(_, essential)| *essential)
            .count();
        assert_eq!(placed, 1, "round {round}");
        assert_eq!(s.orders().await.len(), round + 1);
        assert_eq!(
            (s.stock(s.eggs).await, s.stock(milk).await),
            (Some(0), Some(0))
        );
    }
}
