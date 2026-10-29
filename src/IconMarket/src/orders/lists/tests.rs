//! "คำสั่งซื้อของฉัน" and "ออเดอร์ร้าน" (FR-ORD-10), an order in full, and what each shows of
//! how to reach the other party (FR-ORD-11, FR-SHOP-08, NFR-PRIV-04); a pre-ordered line's ready
//! date (FR-PROD-11). The tests each start a
//! throwaway PostgreSQL, so they need Docker.

use chrono::{NaiveDate, TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::catalog::shops::settings;
use crate::config::{self, Config, SessionConfig};
use crate::line::flex::{Bubble, Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::Templates;
use crate::persistence::entity::product;
use crate::persistence::{orm, random_id, testing};
use crate::thai::Money;
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";
const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_SELLER: &str = "Ue5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";
const OTHER_BUYER: &str = "Uf6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6";

/// What only the seller of an active order sees of its buyer: the house number, the phone and
/// the note
const BUYER_PRIVATE: [&str; 3] = ["45/2", "081-234-5678", "ไม่ใส่ถุง"];
/// What no card shows as it is stored: the buyer's phone, and the latitude they shared, which
/// only the link of the seller's "ดูแผนที่" carries
const BUYER_RAW: [&str; 2] = ["0812345678", "13.75"];
/// What only the buyer of an active order sees of its shop: its phone, as shown, and where to
/// pick it up
const SHOP_PHONE: &str = "089-876-5432";
const PICKUP: &str = "หน้าวัดป่า";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    seller: user::Model,
    buyer: user::Model,
    admin: user::Model,
    other_seller: i64,
    other_buyer: i64,
    shop: shop::Model,
    other_shop: shop::Model,
    eggs: product::Model,
    orders: std::cell::Cell<u32>,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = user::ActiveModel {
        display_name: Set(Some("ป้าแดง".to_owned())),
        ..testing::new_user(SELLER)
    };
    let seller = seller.insert(&db).await.unwrap();
    let buyer = user::ActiveModel {
        display_name: Set(Some("สมชาย".to_owned())),
        zone: Set(Some("ซอย 3".to_owned())),
        house_number: Set(Some("45/2".to_owned())),
        phone: Set(Some("0812345678".to_owned())),
        ..testing::new_user(BUYER)
    };
    let buyer = buyer.insert(&db).await.unwrap();
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(ADMIN)
    };
    let admin = admin.insert(&db).await.unwrap();
    let other_seller = testing::new_user(OTHER_SELLER).insert(&db).await.unwrap();
    let other_shop = shop::ActiveModel {
        name: Set("ร้านลุงชม".to_owned()),
        ..testing::new_shop(other_seller.id)
    };
    let other_shop = other_shop.insert(&db).await.unwrap();
    let other_buyer = testing::new_user(OTHER_BUYER).insert(&db).await.unwrap();
    let shop = shop::ActiveModel {
        phone: Set(Some("0898765432".to_owned())),
        pickup_address: Set(Some(PICKUP.to_owned())),
        pickup_latitude: Set(Some(14.0)),
        pickup_longitude: Set(Some(100.6)),
        ..testing::new_shop(seller.id)
    };
    let shop = shop.insert(&db).await.unwrap();
    let category = testing::new_category("ไข่/นม").insert(&db).await.unwrap();
    let eggs = testing::new_product(shop.id, category.id, "ไข่ไก่เบอร์ 2", Some(6))
        .insert(&db)
        .await
        .unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        seller,
        buyer,
        admin,
        other_seller: other_seller.id,
        other_buyer: other_buyer.id,
        shop,
        other_shop,
        eggs,
        orders: std::cell::Cell::new(0),
    }
}

impl Setup {
    /// An order of the buyer's at the shop, of 2 แผง of eggs, delivered to the buyer, who shared
    /// where, or picked up, now at `status`: each placed a minute after the one before.
    async fn order(&self, status: Status, fulfillment: order::Fulfillment) -> order::Model {
        self.order_with(
            status,
            fulfillment,
            PaymentStatus::Unpaid,
            self.buyer.id,
            self.shop.id,
            1,
        )
        .await
    }

    /// An order as [`order`](Self::order) places it, `payment` as it is paid, by `buyer_id` at
    /// `shop_id`, with `lines` lines.
    async fn order_with(
        &self,
        status: Status,
        fulfillment: order::Fulfillment,
        payment: PaymentStatus,
        buyer_id: i64,
        shop_id: i64,
        lines: usize,
    ) -> order::Model {
        self.orders.set(self.orders.get() + 1);
        let n = self.orders.get();
        let number = format!("IM-20261025-{n:04}");
        let delivered = fulfillment == order::Fulfillment::Delivery;
        let copy = |value: &str| delivered.then(|| value.to_owned());
        let fee = if delivered {
            Money::from_baht(10)
        } else {
            Money::ZERO
        };
        let subtotal = Money::from_baht(220).times(i32::try_from(lines).unwrap());
        let placed = t0() - TimeDelta::hours(2) + TimeDelta::minutes(n.into());
        let order = order::ActiveModel {
            status: Set(status),
            fulfillment: Set(fulfillment),
            delivery_zone: Set(copy("ซอย 3")),
            delivery_house_number: Set(copy("45/2")),
            delivery_latitude: Set(delivered.then_some(13.75)),
            delivery_longitude: Set(delivered.then_some(100.5)),
            note: Set(Some("ไม่ใส่ถุงค่ะ".to_owned())),
            subtotal: Set(subtotal),
            delivery_fee: Set(fee),
            total: Set([subtotal, fee].into_iter().sum()),
            payment_status: Set(payment),
            created_at: Set(placed),
            updated_at: Set(placed),
            ..testing::new_order(buyer_id, shop_id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap();
        for line in 0..lines {
            order_item::ActiveModel {
                order_id: Set(order.id),
                product_id: Set(self.eggs.id),
                name: Set(format!("{} ({})", self.eggs.name, line + 1)),
                unit: Set(self.eggs.unit.clone()),
                unit_price: Set(self.eggs.price),
                quantity: Set(2),
                line_total: Set(Money::from_baht(220)),
                ..Default::default()
            }
            .insert(&self.db)
            .await
            .unwrap();
        }
        order
    }

    /// Handles an event of user `user_id`'s with `handle`, as the worker does, and returns its
    /// reply, which must fit LINE's limits.
    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let messages = turn.finish(messages).messages;
        txn.commit().await.unwrap();
        limits::check(&messages).unwrap();
        messages
    }

    /// User `user_id` taps a button whose postback is `data`, of the lists, or "ดูรายละเอียด".
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| match postback.action() {
            VIEW => view(turn, &postback).await,
            action if action == Item::MyOrders.action() => mine(turn, &postback).await,
            action if action == Item::ShopOrders.action() => shop(turn, &postback).await,
            action => panic!("not a list's: {action}"),
        })
        .await
    }

    /// User `user_id` types "ออเดอร์".
    async fn types_orders(&self, user_id: i64) -> Vec<Message> {
        self.handle(user_id, async |turn| typed(turn).await).await
    }

    /// The seller shows the shop's phone to buyers with an active order, or hides it
    /// (FR-SHOP-08), in "ตั้งค่าร้าน".
    async fn show_phone(&self, show: bool) {
        let data = format!("{}?show={}", settings::PHONE, i32::from(show));
        let postback = Postback::parse(&data).unwrap();
        self.handle(self.seller.id, async |turn| {
            settings::tapped(turn, &self.config, &postback).await
        })
        .await;
    }

    async fn suspend(&self, user_id: i64) {
        user::ActiveModel {
            id: Set(user_id),
            status: Set(user::Status::Suspended),
            ..Default::default()
        }
        .update(&self.db)
        .await
        .unwrap();
    }
}

/// The postback data of "ดูรายละเอียด" of order `id`.
fn details(id: i64) -> String {
    format!("{VIEW}?order={id}")
}

/// The cards of `message`, a carousel without its "ดูเพิ่มเติม", or a single card, and the
/// postback data of its "ดูเพิ่มเติม" if it has one.
fn cards(message: &Message) -> (Vec<&Bubble>, Option<String>) {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } => {
            let mut cards: Vec<&Bubble> = carousel.contents.iter().collect();
            let more = cards.last().and_then(|card| more_of(card));
            if more.is_some() {
                cards.pop();
            }
            (cards, more)
        }
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => (vec![bubble], None),
        body => panic!("not a card: {body:?}"),
    }
}

/// The postback data of `card`'s button if it is "ดูเพิ่มเติม".
fn more_of(card: &Bubble) -> Option<String> {
    let [Component::Button(button)] = &card.body.as_ref()?.contents[..] else {
        return None;
    };
    match &button.action {
        Action::Postback { label, data, .. } if *label == labels::MORE => Some(data.clone()),
        _ => None,
    }
}

/// The one card of `message`.
fn card(message: &Message) -> &Bubble {
    let (cards, more) = cards(message);
    let ([card], None) = (&cards[..], more) else {
        panic!("not one card: {message:?}");
    };
    card
}

/// The texts of `card`'s body, in order, the rows' labels and values among them: its headline
/// first.
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

/// The value of the row labelled `label` in `card`, if there is one.
fn row(card: &Bubble, label: &str) -> Option<String> {
    let texts = texts(card);
    let at = texts.iter().position(|text| text == label)?;
    Some(texts[at + 1].clone())
}

/// The order number in the headline of each of `cards`.
fn numbers(cards: &[&Bubble]) -> Vec<String> {
    cards
        .iter()
        .map(|card| {
            let headline = &texts(card)[0];
            headline.strip_prefix("ออเดอร์ ").expect(headline).to_owned()
        })
        .collect()
}

/// The label and postback data, or link, of each button of `card`.
fn buttons(card: &Bubble) -> Vec<(String, String)> {
    let Some(footer) = &card.footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => (label.as_str().to_owned(), data.clone()),
                Action::Uri { label, uri } => (label.as_str().to_owned(), uri.clone()),
                action => panic!("not a postback or a link: {action:?}"),
            },
            other => panic!("not a button: {other:?}"),
        })
        .collect()
}

fn labels_of(card: &Bubble) -> Vec<String> {
    buttons(card).into_iter().map(|(label, _)| label).collect()
}

/// All `card` says and links to, as LINE gets it.
fn json(card: &Bubble) -> String {
    serde_json::to_string(card).unwrap()
}

fn quick_replies(message: &Message) -> Vec<(&str, String)> {
    let items = message.quick_reply.as_ref().map(|quick| quick.items.iter());
    items
        .into_iter()
        .flatten()
        .map(|item| {
            let data = match &item.action {
                Action::Postback { data, .. } => data.clone(),
                Action::Message { text, .. } => text.clone(),
                action => panic!("not a postback or a message: {action:?}"),
            };
            (item.action.label().as_str(), data)
        })
        .collect()
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The card of order `number` in `message`, a list.
fn card_of<'a>(message: &'a Message, number: &str) -> &'a Bubble {
    let (cards, _) = cards(message);
    let at = numbers(&cards).iter().position(|n| n == number);
    cards[at.unwrap_or_else(|| panic!("{number} not listed"))]
}

/// FR-ORD-10: "คำสั่งซื้อของฉัน" lists the buyer's open orders first, then the rest, the newest
/// first within each, with what the buyer can do about each and "ดูรายละเอียด"; never anyone
/// else's.
#[tokio::test]
async fn the_buyer_sees_open_orders_first_then_history() {
    let s = setup().await;
    let pickup = order::Fulfillment::Pickup;
    let completed = s.order(Status::Completed, pickup).await;
    let pending = s.order(Status::Pending, pickup).await;
    let cancelled = s.order(Status::Cancelled, pickup).await;
    let accepted = s.order(Status::Accepted, pickup).await;
    let expired = s.order(Status::Expired, pickup).await;
    let unpaid = PaymentStatus::Unpaid;
    s.order_with(Status::Pending, pickup, unpaid, s.other_buyer, s.shop.id, 1)
        .await;
    let elsewhere = s
        .order_with(
            Status::Ready,
            pickup,
            unpaid,
            s.buyer.id,
            s.other_shop.id,
            1,
        )
        .await;

    let reply = s.taps(s.buyer.id, "my_orders").await;
    let [list] = &reply[..] else {
        panic!("one message: {reply:?}");
    };
    let (listed, more) = cards(list);
    assert_eq!(more, None);
    let expected = [
        &elsewhere, &accepted, &pending, &expired, &cancelled, &completed,
    ];
    let expected: Vec<String> = expected
        .iter()
        .map(|order| order.order_number.clone())
        .collect();
    assert_eq!(numbers(&listed), expected);

    let card = card_of(list, &pending.order_number);
    assert_eq!(
        texts(card)[..6],
        [
            "ออเดอร์ IM-20261025-0002",
            "สถานะ",
            "รอร้านยืนยัน",
            "สั่งเมื่อ",
            "25 ต.ค. 2569 07:02",
            "ร้านป้าแดง",
        ]
    );
    assert_eq!(
        buttons(card),
        [
            (
                "ยกเลิกออเดอร์".to_owned(),
                format!("order.cancel?order={}", pending.id)
            ),
            ("ดูรายละเอียด".to_owned(), details(pending.id)),
        ]
    );
    assert_eq!(row(card, "รวม").as_deref(), Some("฿220"));
    assert_eq!(row(card, "รับของ").as_deref(), Some("รับเองที่ร้าน"));
    // Completed, yet to be rated (FR-ORD-14)
    let over = card_of(list, &completed.order_number);
    assert_eq!(labels_of(over), ["ให้คะแนน", "ดูรายละเอียด"]);
    assert_eq!(
        quick_replies(list),
        [("เลือกซื้อต่อ", "buy".to_owned()), ("เมนู", "เมนู".to_owned())]
    );
}

/// FR-ORD-10, FR-LINE-11: a buyer with many orders gets them a page at a time; a page past the
/// last, from an old button, starts over.
#[tokio::test]
async fn the_buyers_orders_are_paged() {
    let s = setup().await;
    let mut placed = Vec::new();
    for _ in 0..=ORDERS_PER_PAGE {
        placed.push(s.order(Status::Completed, order::Fulfillment::Pickup).await);
    }
    let first = s.taps(s.buyer.id, "my_orders").await;
    let (listed, more) = cards(&first[0]);
    assert_eq!(listed.len(), ORDERS_PER_PAGE);
    assert_eq!(more.as_deref(), Some("my_orders?page=1"));
    let second = s.taps(s.buyer.id, "my_orders?page=1").await;
    let (listed, more) = cards(&second[0]);
    assert_eq!(numbers(&listed), [placed[0].order_number.clone()]);
    assert_eq!(more, None);
    let past = s.taps(s.buyer.id, "my_orders?page=9").await;
    assert_eq!(past, first);
}

/// FR-UI-03: a buyer without any order is told so, and where to go.
#[tokio::test]
async fn a_buyer_without_orders_is_told_so() {
    let s = setup().await;
    let reply = s.taps(s.other_buyer, "my_orders").await;
    assert_eq!(text(&reply[0]), "ยังไม่มีคำสั่งซื้อค่ะ เลือกซื้อของได้เลยค่ะ");
    assert_eq!(
        quick_replies(&reply[0]),
        [("เลือกซื้อต่อ", "buy".to_owned()), ("เมนู", "เมนู".to_owned())]
    );
}

/// FR-ORD-10, FR-PAY-07: "ออเดอร์ร้าน" lists the shop's orders by filter: new ones and active
/// ones the oldest first, unpaid ones and those over the newest first; "ใหม่" by default. Never
/// another shop's.
#[tokio::test]
async fn the_shop_orders_are_filtered() {
    let s = setup().await;
    let (pickup, delivery) = (order::Fulfillment::Pickup, order::Fulfillment::Delivery);
    let (buyer, shop) = (s.buyer.id, s.shop.id);
    let place = async |status, fulfillment, payment| {
        s.order_with(status, fulfillment, payment, buyer, shop, 1)
            .await
            .order_number
    };
    let new = [
        place(Status::Pending, pickup, PaymentStatus::Unpaid).await,
        place(Status::Pending, delivery, PaymentStatus::Unpaid).await,
    ];
    let active = [
        place(Status::Accepted, pickup, PaymentStatus::Unpaid).await,
        place(Status::Ready, pickup, PaymentStatus::Paid).await,
        place(Status::Delivering, delivery, PaymentStatus::Unpaid).await,
    ];
    let paid = place(Status::Completed, pickup, PaymentStatus::Paid).await;
    let unpaid = place(Status::Completed, pickup, PaymentStatus::Unpaid).await;
    let to_confirm = place(
        Status::Completed,
        delivery,
        PaymentStatus::AwaitingConfirmation,
    )
    .await;
    let rejected = place(Status::Rejected, pickup, PaymentStatus::Unpaid).await;
    let refund = place(Status::Cancelled, pickup, PaymentStatus::RefundDue).await;
    let expired = place(Status::Expired, delivery, PaymentStatus::Unpaid).await;
    let unpaid_other = PaymentStatus::Unpaid;
    s.order_with(
        Status::Pending,
        pickup,
        unpaid_other,
        buyer,
        s.other_shop.id,
        1,
    )
    .await;

    let listed = async |data: &str| {
        let reply = s.taps(s.seller.id, data).await;
        let (cards, more) = cards(reply.last().unwrap());
        assert_eq!(more, None);
        numbers(&cards)
    };
    assert_eq!(listed("shop_orders").await, new);
    assert_eq!(listed("shop_orders?filter=new").await, new);
    assert_eq!(listed("shop_orders?filter=active").await, active);
    // Money still to settle, either way (FR-PAY-07, FR-PAY-09)
    assert_eq!(
        listed("shop_orders?filter=unpaid").await,
        [refund.clone(), to_confirm.clone(), unpaid.clone()]
    );
    assert_eq!(
        listed("shop_orders?filter=done").await,
        [
            expired,
            refund.clone(),
            rejected,
            to_confirm.clone(),
            unpaid.clone(),
            paid
        ]
    );
    // An unknown filter, from no button of ours, is "ใหม่"
    assert_eq!(listed("shop_orders?filter=all").await, new);

    let reply = s.taps(s.seller.id, "shop_orders?filter=unpaid").await;
    let [heading, list] = &reply[..] else {
        panic!("a heading and the list: {reply:?}");
    };
    assert_eq!(text(heading), "ออเดอร์ \"ค้างชำระ\" ของร้านค่ะ");
    let owed = card_of(list, &unpaid);
    assert_eq!(
        row(owed, "ชำระเงิน").as_deref(),
        Some("เงินสดตอนรับของ · ค้างชำระ")
    );
    assert_eq!(labels_of(owed), ["ได้รับเงินแล้ว", "ดูรายละเอียด"]);
    let owed_back = card_of(list, &refund);
    assert_eq!(
        row(owed_back, "ชำระเงิน").as_deref(),
        Some("เงินสดตอนรับของ · รอคืนเงิน")
    );
    assert_eq!(labels_of(owed_back), ["คืนเงินแล้ว", "ดูรายละเอียด"]);
    // Refunded, it is settled
    let refunded = order::ActiveModel {
        payment_status: Set(PaymentStatus::Refunded),
        ..Default::default()
    };
    order::Entity::update_many()
        .set(refunded)
        .filter(order::Column::OrderNumber.eq(&refund))
        .exec(&s.db)
        .await
        .unwrap();
    assert_eq!(
        listed("shop_orders?filter=unpaid").await,
        [to_confirm.clone(), unpaid.clone()]
    );
    assert_eq!(
        quick_replies(list),
        [
            ("ใหม่", "shop_orders?filter=new".to_owned()),
            ("กำลังดำเนินการ", "shop_orders?filter=active".to_owned()),
            ("ค้างชำระ", "shop_orders?filter=unpaid".to_owned()),
            ("เสร็จแล้ว/ยกเลิก", "shop_orders?filter=done".to_owned()),
            ("คำสั่งซื้อของฉัน", "my_orders".to_owned()),
            ("เมนู", "เมนู".to_owned()),
        ]
    );
    // The seller accepts a new order straight from the list (FR-ORD-03)
    let reply = s.taps(s.seller.id, "shop_orders").await;
    let first = card_of(&reply[1], &new[0]);
    assert_eq!(labels_of(first), ["รับออเดอร์", "ปฏิเสธ", "ดูรายละเอียด"]);
    // The buyer's card never says ค้างชำระ, only how far it is paid: it is the seller's to chase
    let owed = order::Entity::find()
        .filter(order::Column::OrderNumber.eq(&unpaid))
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    let theirs = s.taps(s.buyer.id, &details(owed.id)).await;
    let theirs = card(&theirs[0]);
    assert_eq!(
        row(theirs, "ชำระเงิน").as_deref(),
        Some("เงินสดตอนรับของ · ยังไม่ชำระ")
    );
}

/// FR-ORD-10: the shop's orders are paged within their filter, which "ดูเพิ่มเติม" keeps; a
/// filter without any says so.
#[tokio::test]
async fn the_shop_orders_are_paged_within_their_filter() {
    let s = setup().await;
    let mut placed = Vec::new();
    for _ in 0..=ORDERS_PER_PAGE {
        placed.push(s.order(Status::Accepted, order::Fulfillment::Pickup).await);
    }
    let first = s.taps(s.seller.id, "shop_orders?filter=active").await;
    let (listed, more) = cards(&first[1]);
    assert_eq!(listed.len(), ORDERS_PER_PAGE);
    let more = more.unwrap();
    assert_eq!(more, "shop_orders?filter=active&page=1");
    let second = s.taps(s.seller.id, &more).await;
    let (listed, _) = cards(&second[1]);
    assert_eq!(
        numbers(&listed),
        [placed[ORDERS_PER_PAGE].order_number.clone()]
    );

    let none = s.taps(s.seller.id, "shop_orders?filter=unpaid").await;
    let [none] = &none[..] else {
        panic!("one message: {none:?}");
    };
    assert_eq!(text(none), "ตอนนี้ไม่มีออเดอร์ \"ค้างชำระ\"ค่ะ");
    assert_eq!(quick_replies(none).len(), 6);
}

/// NFR-SEC-02: only a shop's owner has "ออเดอร์ร้าน".
#[tokio::test]
async fn only_a_shop_owner_has_shop_orders() {
    let s = setup().await;
    s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let refused = s.taps(s.buyer.id, "shop_orders").await;
    assert_eq!(refused, [s.templates.refused(Denied::NotAllowed, None)]);
    // Another seller sees only their own shop's
    let theirs = s.taps(s.other_seller, "shop_orders").await;
    assert_eq!(text(&theirs[0]), "ตอนนี้ไม่มีออเดอร์ \"ใหม่\"ค่ะ");
}

/// Acceptance of #34, FR-ORD-11, NFR-PRIV-04: the seller sees the buyer's phone, house number,
/// note and location only while the order is active; its display name and zone while it is
/// yet to be accepted; its name alone once it is over. In the list as in full.
#[tokio::test]
async fn a_seller_sees_how_to_reach_the_buyer_only_while_the_order_is_active() {
    let s = setup().await;
    for status in Status::iter() {
        let placed = s.order(status, order::Fulfillment::Delivery).await;
        let number = &placed.order_number;
        let data = format!("shop_orders?filter={}", Filter::of(&placed).key());
        let list = s.taps(s.seller.id, &data).await;
        let listed = card_of(list.last().unwrap(), number);
        let full = s.taps(s.seller.id, &details(placed.id)).await;
        let full = card(&full[0]);
        for card in [listed, full] {
            let all = json(card);
            for private in BUYER_PRIVATE {
                assert_eq!(
                    all.contains(private),
                    status.is_active(),
                    "{private} on a {status:?} order"
                );
            }
            for raw in BUYER_RAW {
                assert!(!all.contains(raw) || card == full, "{raw} in a list");
            }
            assert!(all.contains("สมชาย"), "the name on a {status:?} order");
            let zone = status == Status::Pending || status.is_active();
            assert_eq!(
                all.contains("ซอย 3"),
                zone,
                "the zone on a {status:?} order"
            );
            assert!(!all.contains(SHOP_PHONE), "the shop's phone to its seller");
        }
        let link = buttons(full)
            .into_iter()
            .find(|(label, _)| label == "ดูแผนที่");
        let link = link.map(|(_, uri)| uri);
        let expected = status.is_active().then(|| {
            "https://www.google.com/maps/search/?api=1&query=13.750000,100.500000".to_owned()
        });
        assert_eq!(link, expected, "the map of a {status:?} order");
    }

    // As the seller sees an active one, and one yet to be accepted
    let accepted = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let full = s.taps(s.seller.id, &details(accepted.id)).await;
    let full = card(&full[0]);
    assert_eq!(
        row(full, "รับของ").as_deref(),
        Some("ส่งที่ ซอย 3 บ้านเลขที่ 45/2")
    );
    assert_eq!(row(full, "ลูกค้า").as_deref(), Some("สมชาย"));
    assert_eq!(row(full, "โทร").as_deref(), Some("081-234-5678"));
    assert_eq!(row(full, "ฝากถึงร้าน").as_deref(), Some("ไม่ใส่ถุงค่ะ"));
    let pending = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let list = s.taps(s.seller.id, "shop_orders").await;
    let listed = card_of(&list[1], &pending.order_number);
    assert_eq!(row(listed, "รับของ").as_deref(), Some("รับเองที่ร้าน"));
    assert_eq!(row(listed, "ลูกค้า").as_deref(), Some("สมชาย · ซอย 3"));
    assert_eq!(row(listed, "โทร"), None);
}

/// Acceptance of #34, FR-ORD-11, FR-SHOP-08: the buyer of an active order sees the shop's phone
/// unless its seller hides it, and where to pick it up for pickup; of any other order, neither.
#[tokio::test]
async fn hiding_the_shop_phone_removes_it_from_the_buyers_active_order_view() {
    let s = setup().await;
    let (pickup, delivery) = (order::Fulfillment::Pickup, order::Fulfillment::Delivery);
    let ready = s.order(Status::Ready, pickup).await;
    let delivering = s.order(Status::Delivering, delivery).await;
    let pending = s.order(Status::Pending, pickup).await;
    let completed = s.order(Status::Completed, pickup).await;
    let cancelled = s.order(Status::Cancelled, pickup).await;
    // The buyer's views of `order`: its card in the list, and in full
    let views = async |order: &order::Model| {
        let list = s.taps(s.buyer.id, "my_orders").await;
        let listed = card_of(&list[0], &order.order_number).clone();
        let full = s.taps(s.buyer.id, &details(order.id)).await;
        [listed, card(&full[0]).clone()]
    };

    for view in views(&ready).await {
        assert_eq!(row(&view, "เบอร์ร้าน").as_deref(), Some(SHOP_PHONE));
        assert_eq!(row(&view, "ที่รับของ").as_deref(), Some(PICKUP));
    }
    let [_, full] = views(&ready).await;
    let map = "https://www.google.com/maps/search/?api=1&query=14.000000,100.600000";
    assert_eq!(
        buttons(&full),
        [
            (
                "ได้รับของแล้ว".to_owned(),
                format!("order.received?order={}", ready.id)
            ),
            (
                "ขอยกเลิก".to_owned(),
                format!("order.request_cancel?order={}", ready.id)
            ),
            (
                "ส่งข้อความถึงร้าน".to_owned(),
                format!("order.message_shop?order={}", ready.id)
            ),
            ("ดูแผนที่".to_owned(), map.to_owned()),
        ]
    );
    // Delivered: the phone, but not where the shop is
    for view in views(&delivering).await {
        let all = json(&view);
        assert!(all.contains(SHOP_PHONE));
        assert!(!all.contains(PICKUP));
        assert!(!all.contains("ดูแผนที่"));
    }
    for order in [&pending, &completed, &cancelled] {
        for view in views(order).await {
            let all = json(&view);
            for shown in [SHOP_PHONE, "0898765432", PICKUP, "ดูแผนที่"] {
                assert!(
                    !all.contains(shown),
                    "{shown} on a {:?} order",
                    order.status
                );
            }
        }
    }
    // Nor does the buyer ever see their own details as if they were the seller's to see
    for view in views(&ready).await {
        for private in BUYER_PRIVATE.into_iter().chain(BUYER_RAW) {
            assert!(!json(&view).contains(private), "{private} to the buyer");
        }
    }

    s.show_phone(false).await;
    for order in [&ready, &delivering] {
        for view in views(order).await {
            let all = json(&view);
            assert!(
                !all.contains(SHOP_PHONE),
                "the phone hidden on {:?}",
                order.status
            );
            assert!(!all.contains("0898765432"));
        }
    }
    for view in views(&ready).await {
        assert_eq!(row(&view, "ที่รับของ").as_deref(), Some(PICKUP));
    }
    s.show_phone(true).await;
    for view in views(&ready).await {
        assert_eq!(row(&view, "เบอร์ร้าน").as_deref(), Some(SHOP_PHONE));
    }
}

/// FR-ORD-10, AC-17: a list's card shows an order's first lines and counts the rest; in full,
/// it shows every line and its subtotal, and returns to the list it is in.
#[tokio::test]
async fn an_order_in_full() {
    let s = setup().await;
    let unpaid = PaymentStatus::Unpaid;
    let (buyer, shop) = (s.buyer.id, s.shop.id);
    let delivery = order::Fulfillment::Delivery;
    let placed = s
        .order_with(Status::Accepted, delivery, unpaid, buyer, shop, 5)
        .await;
    let list = s.taps(s.seller.id, "shop_orders?filter=active").await;
    let listed = texts(card_of(&list[1], &placed.order_number));
    let names: Vec<&String> = listed
        .iter()
        .filter(|text| text.starts_with("ไข่ไก่"))
        .collect();
    assert_eq!(names.len(), 3);
    assert!(listed.contains(&"และอีก 2 รายการ".to_owned()));
    assert!(!listed.contains(&"ค่าสินค้า".to_owned()));

    let full = s.taps(s.seller.id, &details(placed.id)).await;
    let [full] = &full[..] else {
        panic!("one message: {full:?}");
    };
    let card = card(full);
    let names = texts(card)
        .into_iter()
        .filter(|text| text.starts_with("ไข่ไก่"));
    assert_eq!(names.count(), 5);
    assert_eq!(row(card, "ค่าสินค้า").as_deref(), Some("฿1,100"));
    assert_eq!(row(card, "ค่าส่ง").as_deref(), Some("฿10"));
    assert_eq!(row(card, "รวม").as_deref(), Some("฿1,110"));
    assert_eq!(
        labels_of(card),
        ["กำลังไปส่ง", "ยกเลิกออเดอร์", "ส่งข้อความถึงลูกค้า", "ดูแผนที่"]
    );
    assert_eq!(
        quick_replies(full),
        [
            ("ออเดอร์ร้าน", "shop_orders?filter=active".to_owned()),
            ("เมนู", "เมนู".to_owned()),
        ]
    );
    let theirs = s.taps(s.buyer.id, &details(placed.id)).await;
    assert_eq!(
        quick_replies(&theirs[0]),
        [
            ("คำสั่งซื้อของฉัน", "my_orders".to_owned()),
            ("เมนู", "เมนู".to_owned())
        ]
    );
}

/// FR-PROD-11, DR-03: a line ordered in advance says when it is ready, on the buyer's list and
/// the order in full, as it was ordered: its product is no pre-order now.
#[tokio::test]
async fn a_pre_ordered_line_says_when_it_is_ready() {
    let s = setup().await;
    let pickup = order::Fulfillment::Pickup;
    let placed = s.order(Status::Accepted, pickup).await;
    order_item::Entity::update_many()
        .col_expr(
            order_item::Column::ReadyOn,
            Expr::value(NaiveDate::from_ymd_opt(2026, 11, 5)),
        )
        .filter(order_item::Column::OrderId.eq(placed.id))
        .exec(&s.db)
        .await
        .unwrap();
    assert_eq!(s.eggs.ready_on, None);
    let ready = "สั่งล่วงหน้า · พร้อม 5 พ.ย. 2569".to_owned();

    let list = s.taps(s.buyer.id, "my_orders").await;
    let listed = texts(card_of(&list[0], &placed.order_number));
    let at = listed.iter().position(|text| text == "฿220").unwrap();
    assert_eq!(listed[at + 1], ready);
    let full = s.taps(s.seller.id, &details(placed.id)).await;
    assert!(texts(card(&full[0])).contains(&ready));
}

/// NFR-SEC-02, FR-ADM-04: only an order's buyer, its seller or an admin sees it in full; an
/// admin sees the card they find it by. A suspended user still sees their orders.
#[tokio::test]
async fn only_the_orders_parties_see_it_in_full() {
    let s = setup().await;
    let placed = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let refused = vec![s.templates.refused(Denied::NotAllowed, None)];
    for user_id in [s.other_buyer, s.other_seller] {
        assert_eq!(s.taps(user_id, &details(placed.id)).await, refused);
    }
    let gone = s.taps(s.buyer.id, &details(i64::MAX)).await;
    assert_eq!(gone, [s.templates.denied(Denied::NotFound)]);
    let found = s.taps(s.admin.id, &details(placed.id)).await;
    let expected = actions::found(&s.db, &s.templates, &placed).await.unwrap();
    assert_eq!(found, [expected]);

    s.suspend(s.buyer.id).await;
    let list = s.taps(s.buyer.id, "my_orders").await;
    assert_eq!(numbers(&cards(&list[0]).0), [placed.order_number.as_str()]);
    let full = s.taps(s.buyer.id, &details(placed.id)).await;
    assert_eq!(numbers(&[card(&full[0])]), [placed.order_number.as_str()]);
    s.suspend(s.seller.id).await;
    let list = s.taps(s.seller.id, "shop_orders?filter=active").await;
    assert_eq!(numbers(&cards(&list[1]).0), [placed.order_number.as_str()]);
    // A suspended admin isn't one for this (FR-ADM-04)
    s.suspend(s.admin.id).await;
    assert_eq!(s.taps(s.admin.id, &details(placed.id)).await, refused);
}

/// FR-UI-12: "ออเดอร์" shows a seller their shop's new orders, with the way to their own; anyone
/// else, or the owner of a shop yet to be approved, their own orders.
#[tokio::test]
async fn the_keyword_shows_a_seller_their_new_orders() {
    let s = setup().await;
    let placed = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let seller = s.types_orders(s.seller.id).await;
    assert_eq!(seller, s.taps(s.seller.id, "shop_orders?filter=new").await);
    assert_eq!(
        numbers(&cards(&seller[1]).0),
        [placed.order_number.as_str()]
    );
    let buyer = s.types_orders(s.buyer.id).await;
    assert_eq!(buyer, s.taps(s.buyer.id, "my_orders").await);

    let applicant = testing::new_user("U0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a")
        .insert(&s.db)
        .await
        .unwrap();
    let applying = shop::ActiveModel {
        status: Set(shop::Status::Pending),
        ..testing::new_shop(applicant.id)
    };
    applying.insert(&s.db).await.unwrap();
    let reply = s.types_orders(applicant.id).await;
    assert_eq!(reply, s.taps(applicant.id, "my_orders").await);

    // A seller's own orders offer the way back to the shop's
    let own = s.taps(s.seller.id, "my_orders").await;
    assert_eq!(
        quick_replies(&own[0]),
        [
            ("เลือกซื้อต่อ", "buy".to_owned()),
            ("ออเดอร์ร้าน", "shop_orders?filter=new".to_owned()),
            ("เมนู", "เมนู".to_owned()),
        ]
    );
}
