//! Acting on an order (Table 4.6-B): the seller is told of a new order and moves it on, as in
//! Appendix A.4; rejecting and cancelling ask why; the buyer cancels, asks to cancel or confirms
//! receipt; an admin cancels. Each move is told to the other party, only the order's parties
//! can act, and a button of an order that moved on shows it as it is (FR-UI-13). Each event is
//! handled in a transaction committed once it is handled, with what it notifies queued in it.
//! The tests each start a throwaway PostgreSQL, so they need Docker.

use std::cell::Cell;

use chrono::{TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, Config, NotificationConfig, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates;
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{audit_log, order_event, product, push_log, session};
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

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    /// The switches the events are handled with
    switches: Cell<NotificationConfig>,
    templates: Templates,
    seller: user::Model,
    buyer: user::Model,
    admin: user::Model,
    other_seller: i64,
    other_buyer: i64,
    shop: shop::Model,
    eggs: product::Model,
    orders: Cell<u32>,
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
    other_shop.insert(&db).await.unwrap();
    let other_buyer = testing::new_user(OTHER_BUYER).insert(&db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&db).await.unwrap();
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
        switches: Cell::new(config.notification),
        config,
        seller,
        buyer,
        admin,
        other_seller: other_seller.id,
        other_buyer: other_buyer.id,
        shop,
        eggs,
        orders: Cell::new(0),
    }
}

impl Setup {
    /// An order of 2 แผง of eggs, their stock reserved, delivered to the buyer or picked up,
    /// now at `status`.
    async fn order(&self, status: Status, fulfillment: order::Fulfillment) -> order::Model {
        self.orders.set(self.orders.get() + 1);
        let number = format!("IM-20261025-{:04}", self.orders.get());
        let delivered = fulfillment == order::Fulfillment::Delivery;
        let copy = |value: &str| delivered.then(|| value.to_owned());
        let fee = if delivered {
            Money::from_baht(10)
        } else {
            Money::ZERO
        };
        let order = order::ActiveModel {
            status: Set(status),
            fulfillment: Set(fulfillment),
            delivery_zone: Set(copy("ซอย 3")),
            delivery_house_number: Set(copy("45/2")),
            note: Set(Some("ไม่ใส่ถุงค่ะ".to_owned())),
            delivery_fee: Set(fee),
            total: Set([Money::from_baht(220), fee].into_iter().sum()),
            ..testing::new_order(self.buyer.id, self.shop.id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap();
        order_item::ActiveModel {
            order_id: Set(order.id),
            product_id: Set(self.eggs.id),
            name: Set(self.eggs.name.clone()),
            unit: Set(self.eggs.unit.clone()),
            unit_price: Set(self.eggs.price),
            quantity: Set(2),
            line_total: Set(Money::from_baht(220)),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .unwrap();
        order
    }

    /// Handles an event of user `user_id`'s with `handle`, as the worker does. Returns the
    /// reply, which must fit LINE's limits if there is one, and whether it is essential.
    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> (Vec<Message>, bool) {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages,
            outbox,
            essential,
            ..
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.switches.get(), t0())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        (messages, essential)
    }

    async fn tapped(&self, user_id: i64, data: &str) -> (Vec<Message>, bool) {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            tapped(turn, &self.config, &postback).await
        })
        .await
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.tapped(user_id, data).await.0
    }

    /// Order `id` in full, as user `user_id` sees it from "ดูรายละเอียด", as LINE gets it.
    async fn details(&self, user_id: i64, id: i64) -> String {
        let postback = Postback::parse(&data(crate::orders::VIEW, id)).unwrap();
        let (shown, _) = self
            .handle(user_id, async |turn| {
                crate::orders::lists::view(turn, &postback).await
            })
            .await;
        serde_json::to_string(&shown).unwrap()
    }

    /// User `user_id` types `text` as the reason asked for.
    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        let (messages, _) = self
            .handle(user_id, async |turn| {
                let session = turn
                    .flow
                    .as_ref()
                    .and_then(|flow| flow.read::<ReasonFlow>());
                let session = session.expect("giving a reason");
                let answered = answer(turn, &self.config, session, &input).await?;
                Ok(answered.expect("an answer"))
            })
            .await;
        messages
    }

    async fn in_flow(&self, user_id: i64) -> bool {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .is_some()
    }

    async fn get(&self, id: i64) -> order::Model {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// `order`, paid by `method`, now `payment`.
    async fn paid_by(
        &self,
        order: &order::Model,
        method: PaymentMethod,
        payment: PaymentStatus,
    ) -> order::Model {
        let mut row: order::ActiveModel = order.clone().into();
        row.payment_method = Set(method);
        row.payment_status = Set(payment);
        row.update(&self.db).await.unwrap()
    }

    /// The shop takes PromptPay to `id`, or no longer does.
    async fn set_promptpay(&self, id: Option<&str>) {
        let row = shop::ActiveModel {
            id: Unchanged(self.shop.id),
            promptpay_id: Set(id.map(str::to_owned)),
            ..Default::default()
        };
        row.update(&self.db).await.unwrap();
    }

    /// The PromptPay QR codes stored for order `id` (FR-PAY-04).
    async fn qr_codes(&self, id: i64) -> Vec<image::Model> {
        image::Entity::find()
            .filter(image::Column::OwnerType.eq(OwnerType::PaymentQr))
            .filter(image::Column::OwnerId.eq(id))
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn stock(&self) -> Option<i32> {
        let eggs = product::Entity::find_by_id(self.eggs.id).one(&self.db);
        eggs.await.unwrap().unwrap().stock
    }

    /// Order `id`'s moves, as (from, to, actor type, reason), oldest first.
    async fn history(&self, id: i64) -> Vec<(Option<Status>, Status, ActorType, Option<String>)> {
        order_event::Entity::find()
            .filter(order_event::Column::OrderId.eq(id))
            .order_by_asc(order_event::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|event| {
                (
                    event.from_status,
                    event.to_status,
                    event.actor_type,
                    event.reason,
                )
            })
            .collect()
    }

    /// Every push queued, as (type, recipient, messages), oldest first.
    async fn pushes(&self) -> Vec<(String, String, Option<Json>)> {
        push_log::Entity::find()
            .order_by_asc(push_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.push_type, row.recipient, row.messages))
            .collect()
    }

    async fn forget_pushes(&self) {
        push_log::Entity::delete_many()
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Every audit entry, as (actor, action, target type, target, details).
    async fn audited(&self) -> Vec<(Option<i64>, String, String, Option<i64>, Json)> {
        audit_log::Entity::find()
            .order_by_asc(audit_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.actor_id,
                    row.action,
                    row.target_type,
                    row.target_id,
                    row.details,
                )
            })
            .collect()
    }
}

/// The postback data of `action` on order `id`.
fn data(action: &str, id: i64) -> String {
    format!("{action}?order={id}")
}

/// The messages of one push, as queued.
fn queued(messages: &[Message]) -> Option<Json> {
    Some(serde_json::to_value(messages).unwrap())
}

fn push(kind: &str, to: &str, message: Message) -> (String, String, Option<Json>) {
    (kind.to_owned(), to.to_owned(), queued(&[message]))
}

/// The lines of a notice: its headline, then its details.
fn lines(message: &Message) -> Vec<&str> {
    let Body::Flex {
        contents: FlexContainer::Bubble(card),
        ..
    } = &message.body
    else {
        panic!("not a card: {message:?}");
    };
    let lines = card.body.as_ref().unwrap().contents.iter();
    lines
        .map(|component| match component {
            Component::Text(text) => text.text.as_str(),
            other => panic!("not text: {other:?}"),
        })
        .collect()
}

/// The buttons of a notice, as (label, postback data).
fn buttons_of(message: &Message) -> Vec<(&str, &str)> {
    let Body::Flex {
        contents: FlexContainer::Bubble(card),
        ..
    } = &message.body
    else {
        panic!("not a card: {message:?}");
    };
    let Some(footer) = &card.footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => (label.as_str(), data.as_str()),
                action => panic!("not a postback: {action:?}"),
            },
            other => panic!("not a button: {other:?}"),
        })
        .collect()
}

/// The notice a push holds, as LINE gets it: its lines, headline first, and its buttons.
struct Pushed {
    lines: Vec<String>,
    buttons: Vec<(String, String)>,
}

impl Pushed {
    fn of(messages: &Option<Json>) -> Self {
        let card = &messages.as_ref().expect("messages")[0]["contents"];
        let texts = card["body"]["contents"].as_array().unwrap().iter();
        let lines = texts.map(|text| text["text"].as_str().unwrap().to_owned());
        let footer = card["footer"]["contents"].as_array();
        let buttons = footer.into_iter().flatten().map(|button| {
            let action = &button["action"];
            let field = |name: &str| action[name].as_str().unwrap().to_owned();
            (field("label"), field("data"))
        });
        Self {
            lines: lines.collect(),
            buttons: buttons.collect(),
        }
    }

    fn buttons(&self) -> Vec<(&str, &str)> {
        let buttons = self.buttons.iter();
        buttons
            .map(|(label, data)| (label.as_str(), data.as_str()))
            .collect()
    }
}

fn quick_replies(message: &Message) -> Vec<&str> {
    message
        .quick_reply
        .as_ref()
        .map(|quick_reply| quick_reply.items.iter())
        .into_iter()
        .flatten()
        .map(|item| item.action.label().as_str())
        .collect()
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// FR-ORD-03, FR-ORD-11, Appendix A.4: a new order is pushed to its seller with its lines,
/// delivery fee and total, how it is received and paid, and the buyer's name and zone, but not
/// their house number, phone or note, with "รับออเดอร์" and "ปฏิเสธ"; unless
/// `notification.newOrderToSeller` is off.
#[tokio::test]
async fn a_new_order_is_pushed_to_its_seller_as_in_appendix_a4() {
    let s = setup().await;
    let placed = s.order(Status::Pending, order::Fulfillment::Delivery).await;
    let notify = async |s: &Setup| {
        let items = order_item::Entity::find()
            .filter(order_item::Column::OrderId.eq(placed.id))
            .all(&s.db)
            .await
            .unwrap();
        s.handle(s.buyer.id, async |turn| {
            notify_new_order(turn, &placed, &items, &s.shop, &s.buyer);
            Ok(Vec::new())
        })
        .await;
    };
    notify(&s).await;

    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    let (kind, to, messages) = &pushes[0];
    assert_eq!((kind.as_str(), to.as_str()), ("order.new", SELLER));
    let notice = Pushed::of(messages);
    assert_eq!(
        notice.lines,
        [
            "ออเดอร์ใหม่ค่ะ IM-20261025-0001",
            "ไข่ไก่เบอร์ 2 × 2 แผง = ฿220",
            "ค่าส่ง ฿10 · รวม ฿230",
            "ส่งที่ ซอย 3 · เงินสดตอนรับของ",
            "ลูกค้า: สมชาย",
        ]
    );
    assert_eq!(
        notice.buttons(),
        [
            ("รับออเดอร์", data(ACCEPT, placed.id).as_str()),
            ("ปฏิเสธ", data(REJECT, placed.id).as_str()),
        ]
    );
    let all = messages.as_ref().unwrap().to_string();
    for private in ["45/2", "081", "ไม่ใส่ถุง"] {
        assert!(!all.contains(private), "{private} shown before accepting");
    }

    // A pickup order shows the buyer's zone beside their name
    let pickup = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let new = NewOrder {
        lines: Vec::new(),
        delivery_fee: Money::ZERO,
        total: pickup.total,
        fulfillment: pickup.fulfillment,
        payment: pickup.payment_method,
        buyer: Some("สมชาย"),
        zone: Some("ซอย 3"),
    };
    let card = s
        .templates
        .new_order_notice("IM-20261025-0002", &new, Vec::new());
    assert_eq!(
        lines(&card)[1..],
        [
            "รวม ฿220",
            "รับเองที่ร้าน · เงินสดตอนรับของ",
            "ลูกค้า: สมชาย · ซอย 3"
        ]
    );

    s.forget_pushes().await;
    s.switches.set(NotificationConfig {
        new_order_to_seller: false,
        ..s.config.notification
    });
    notify(&s).await;
    assert_eq!(s.pushes().await, []);
}

/// Appendix A.4, FR-ORD-08, FR-ORD-11: the seller accepts, and sees where to deliver and the
/// buyer's phone and note, with "กำลังไปส่ง" and "ยกเลิกออเดอร์"; the buyer is told, with
/// "ขอยกเลิก". Tapping again changes nothing and shows the order as it is (FR-UI-13).
#[tokio::test]
async fn the_seller_accepts_as_in_appendix_a4() {
    let s = setup().await;
    let placed = s.order(Status::Pending, order::Fulfillment::Delivery).await;
    let (reply, essential) = s.tapped(s.seller.id, &data(ACCEPT, placed.id)).await;
    assert!(essential);
    let order = s.get(placed.id).await;
    assert_eq!(order.status, Status::Accepted);
    assert_eq!(order.accepted_at, Some(t0()));
    assert_eq!(
        lines(&reply[0]),
        [
            "รับออเดอร์ IM-20261025-0001 แล้วค่ะ",
            "ส่งที่ ซอย 3 บ้านเลขที่ 45/2",
            "ลูกค้า: สมชาย",
            "โทร 081-234-5678",
            "ฝากถึงร้าน: ไม่ใส่ถุงค่ะ",
        ]
    );
    assert_eq!(
        buttons_of(&reply[0]),
        [
            ("กำลังไปส่ง", data(DELIVERING, placed.id).as_str()),
            ("ยกเลิกออเดอร์", data(SELLER_CANCEL, placed.id).as_str()),
            ("ส่งข้อความถึงลูกค้า", data(MESSAGE_BUYER, placed.id).as_str()),
        ]
    );
    let request = Postback::new(REQUEST_CANCEL).with_int(ORDER_ID, placed.id);
    let told = s.templates.order_accepted_notice(
        &s.shop.name,
        &order.order_number,
        None,
        vec![
            request.button(labels::REQUEST_CANCEL.clone()),
            relay::button(&order, relay::Side::Buyer),
        ],
    );
    assert_eq!(
        lines(&told),
        ["ร้านป้าแดงรับออเดอร์ IM-20261025-0001 แล้วค่ะ กำลังเตรียมของ"]
    );
    assert_eq!(s.pushes().await, [push("order.status", BUYER, told)]);
    assert_eq!(
        s.history(placed.id).await,
        [(
            Some(Status::Pending),
            Status::Accepted,
            ActorType::Seller,
            None
        )]
    );
    assert_eq!(s.stock().await, Some(6));

    let again = s.taps(s.seller.id, &data(ACCEPT, placed.id)).await;
    assert_eq!(
        lines(&again[0]),
        ["ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"ร้านรับออเดอร์แล้ว\"ค่ะ"]
    );
    assert_eq!(buttons_of(&again[0]), buttons_of(&reply[0]));
    assert_eq!(s.pushes().await.len(), 1);
    assert_eq!(s.history(placed.id).await.len(), 1);
}

/// Table 4.6-B: the seller moves a pickup order to ready, then hands it over; a delivery order
/// goes on its way. The buyer is told of each step with what they can do then; a step that
/// doesn't apply to how the order is received shows it as it is.
#[tokio::test]
async fn the_seller_moves_an_order_on_until_handed_over() {
    let s = setup().await;
    let pickup = s.order(Status::Accepted, order::Fulfillment::Pickup).await;
    let id = pickup.id;
    let wrong_way = s.taps(s.seller.id, &data(DELIVERING, id)).await;
    assert_eq!(
        lines(&wrong_way[0]),
        ["ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"ร้านรับออเดอร์แล้ว\"ค่ะ"]
    );
    assert_eq!(
        buttons_of(&wrong_way[0]),
        [
            ("พร้อมให้รับ", data(READY, id).as_str()),
            ("ยกเลิกออเดอร์", data(SELLER_CANCEL, id).as_str()),
            ("ส่งข้อความถึงลูกค้า", data(MESSAGE_BUYER, id).as_str()),
        ]
    );

    let ready = s.taps(s.seller.id, &data(READY, id)).await;
    assert_eq!(lines(&ready[0]), ["ออเดอร์ IM-20261025-0001 พร้อมให้รับแล้วค่ะ"]);
    assert_eq!(
        buttons_of(&ready[0]),
        [
            ("ส่งมอบแล้ว", data(COMPLETE, id).as_str()),
            ("ยกเลิกออเดอร์", data(SELLER_CANCEL, id).as_str()),
            ("ส่งข้อความถึงลูกค้า", data(MESSAGE_BUYER, id).as_str()),
        ]
    );
    let pushes = s.pushes().await;
    let told = Pushed::of(&pushes[0].2);
    assert_eq!(
        told.lines,
        ["ออเดอร์ IM-20261025-0001 พร้อมให้รับที่ร้านป้าแดงแล้วค่ะ"]
    );
    assert_eq!(
        told.buttons(),
        [
            ("ได้รับของแล้ว", data(RECEIVED, id).as_str()),
            ("ขอยกเลิก", data(REQUEST_CANCEL, id).as_str()),
            ("ส่งข้อความถึงร้าน", data(MESSAGE_SHOP, id).as_str()),
        ]
    );

    // Handed over unpaid, the seller is asked whether they got the money (FR-PAY-07)
    let handed = s.taps(s.seller.id, &data(COMPLETE, id)).await;
    assert_eq!(
        lines(&handed[0]),
        [
            "ออเดอร์ IM-20261025-0001 สำเร็จแล้วค่ะ",
            "ได้รับเงิน ฿220 แล้วหรือยังคะ",
        ]
    );
    assert_eq!(
        buttons_of(&handed[0]),
        [
            ("ได้รับเงินแล้ว", data(MARK_PAID, id).as_str()),
            ("ยังไม่ได้รับ", data(NOT_PAID, id).as_str()),
        ]
    );
    let order = s.get(id).await;
    assert_eq!(
        (order.status, order.completed_at),
        (Status::Completed, Some(t0()))
    );
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 2);
    let told = Pushed::of(&pushes[1].2);
    assert_eq!(
        told.lines,
        ["ออเดอร์ IM-20261025-0001 สำเร็จแล้ว ขอบคุณที่อุดหนุนร้านป้าแดงค่ะ"]
    );
    assert_eq!(s.stock().await, Some(6));

    let delivery = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let on_its_way = s.taps(s.seller.id, &data(DELIVERING, delivery.id)).await;
    assert_eq!(
        lines(&on_its_way[0]),
        ["ออเดอร์ IM-20261025-0002 กำลังไปส่งค่ะ"]
    );
    let pushes = s.pushes().await;
    let told = Pushed::of(&pushes[2].2);
    assert_eq!(told.lines, ["ร้านป้าแดงกำลังไปส่งออเดอร์ IM-20261025-0002ค่ะ"]);
    assert_eq!(s.get(delivery.id).await.delivering_at, Some(t0()));
}

/// FR-ORD-04: rejecting asks why, offering สินค้าหมด, ไม่สะดวกส่ง, ร้านปิดแล้ว and อื่นๆ, which asks
/// for it to be typed; a reason typed straight away is taken too. The order is rejected for it,
/// its stock given back, and the buyer told why.
#[tokio::test]
async fn the_seller_rejects_for_a_reason_picked_or_typed() {
    let s = setup().await;
    let first = s.order(Status::Pending, order::Fulfillment::Delivery).await;
    let asked = s.taps(s.seller.id, &data(REJECT, first.id)).await;
    assert_eq!(
        text(&asked[0]),
        "ปฏิเสธออเดอร์ IM-20261025-0001 เพราะอะไรคะ เลือกด้านล่างได้เลย ลูกค้าจะเห็นเหตุผลนี้ค่ะ"
    );
    assert_eq!(
        quick_replies(&asked[0]),
        ["สินค้าหมด", "ไม่สะดวกส่ง", "ร้านปิดแล้ว", "อื่นๆ", "ยกเลิก"]
    );
    assert!(s.in_flow(s.seller.id).await);
    assert_eq!(s.get(first.id).await.status, Status::Pending);

    let rejected = s.types(s.seller.id, "ไม่สะดวกส่ง").await;
    assert_eq!(
        lines(&rejected[0]),
        [
            "ปฏิเสธออเดอร์ IM-20261025-0001 แล้วค่ะ",
            "คืนจำนวนสินค้าเข้าสต็อกแล้วค่ะ",
        ]
    );
    assert!(!s.in_flow(s.seller.id).await);
    let order = s.get(first.id).await;
    assert_eq!(order.status, Status::Rejected);
    assert_eq!(order.cancel_reason.as_deref(), Some("ไม่สะดวกส่ง"));
    assert_eq!(s.stock().await, Some(8));
    let carry_on = vec![cart::continue_shopping()];
    let told =
        s.templates
            .order_rejected_notice(&s.shop.name, &order.order_number, "ไม่สะดวกส่ง", carry_on);
    assert_eq!(
        lines(&told),
        [
            "ร้านป้าแดงไม่รับออเดอร์ IM-20261025-0001ค่ะ",
            "เหตุผล: ไม่สะดวกส่ง",
            "เลือกซื้อสินค้าอื่นได้เลยค่ะ",
        ]
    );
    assert_eq!(s.pushes().await, [push("order.status", BUYER, told)]);
    assert_eq!(
        s.history(first.id).await,
        [(
            Some(Status::Pending),
            Status::Rejected,
            ActorType::Seller,
            Some("ไม่สะดวกส่ง".to_owned())
        )]
    );

    // อื่นๆ asks for it to be typed, within the limit
    let second = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    s.taps(s.seller.id, &data(REJECT, second.id)).await;
    let other = s.types(s.seller.id, "อื่นๆ").await;
    assert_eq!(text(&other[0]), "พิมพ์เหตุผลมาได้เลยค่ะ ไม่เกิน 300 ตัวอักษร");
    assert_eq!(quick_replies(&other[0]), ["ยกเลิก"]);
    let too_long = s.types(s.seller.id, &"ก".repeat(301)).await;
    assert_eq!(
        too_long[0],
        templates::with_cancel(s.templates.text_too_long(300))
    );
    assert_eq!(s.get(second.id).await.status, Status::Pending);
    s.types(s.seller.id, "  ไข่แตก   หมดเลย ").await;
    let order = s.get(second.id).await;
    assert_eq!(order.cancel_reason.as_deref(), Some("ไข่แตก หมดเลย"));

    // Typed without อื่นๆ
    let third = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    s.taps(s.seller.id, &data(REJECT, third.id)).await;
    s.types(s.seller.id, "วันนี้ไม่อยู่บ้าน").await;
    let order = s.get(third.id).await;
    assert_eq!(order.status, Status::Rejected);
    assert_eq!(order.cancel_reason.as_deref(), Some("วันนี้ไม่อยู่บ้าน"));
}

/// FR-ORD-02, FR-UI-13: an order the buyer cancelled while its seller was saying why they
/// reject it stays cancelled; the seller is shown it as it is.
#[tokio::test]
async fn an_order_that_moved_while_the_reason_was_asked_stays_as_it_is() {
    let s = setup().await;
    let placed = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    s.taps(s.seller.id, &data(REJECT, placed.id)).await;
    s.taps(s.buyer.id, &data(CANCEL_ORDER, placed.id)).await;
    let late = s.types(s.seller.id, "สินค้าหมด").await;
    assert_eq!(
        lines(&late[0]),
        ["ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"ยกเลิกแล้ว\"ค่ะ"]
    );
    assert_eq!(buttons_of(&late[0]), []);
    assert!(!s.in_flow(s.seller.id).await);
    let order = s.get(placed.id).await;
    assert_eq!(
        (order.status, order.cancel_reason),
        (Status::Cancelled, None)
    );
    assert_eq!(s.history(placed.id).await.len(), 1);
    // Given back once
    assert_eq!(s.stock().await, Some(8));

    // Rejecting what moved on shows it, and asks nothing
    let asked = s.taps(s.seller.id, &data(REJECT, placed.id)).await;
    assert_eq!(lines(&asked[0]), lines(&late[0]));
    assert!(!s.in_flow(s.seller.id).await);
}

/// FR-ORD-06, Table 4.8-A: the buyer cancels an order the shop is yet to accept, its stock is
/// given back and the seller told; once accepted, cancelling shows it as it is, with "ขอยกเลิก".
#[tokio::test]
async fn the_buyer_cancels_until_the_shop_accepts() {
    let s = setup().await;
    let placed = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let (cancelled, essential) = s.tapped(s.buyer.id, &data(CANCEL_ORDER, placed.id)).await;
    assert!(essential);
    assert_eq!(lines(&cancelled[0]), ["ยกเลิกออเดอร์ IM-20261025-0001 แล้วค่ะ"]);
    assert_eq!(quick_replies(&cancelled[0]), ["เลือกซื้อต่อ", "เมนู"]);
    let order = s.get(placed.id).await;
    assert_eq!(
        (order.status, order.cancelled_at),
        (Status::Cancelled, Some(t0()))
    );
    assert_eq!(s.stock().await, Some(8));
    let told = s
        .templates
        .order_cancelled_by_buyer_notice(&order.order_number);
    assert_eq!(
        lines(&told),
        [
            "ลูกค้ายกเลิกออเดอร์ IM-20261025-0001ค่ะ",
            "ไม่ต้องเตรียมของ คืนจำนวนสินค้าเข้าสต็อกแล้วค่ะ",
        ]
    );
    assert_eq!(s.pushes().await, [push("order.cancelled", SELLER, told)]);
    assert_eq!(
        s.history(placed.id).await,
        [(
            Some(Status::Pending),
            Status::Cancelled,
            ActorType::Buyer,
            None
        )]
    );

    let accepted = s.order(Status::Accepted, order::Fulfillment::Pickup).await;
    let refused = s.taps(s.buyer.id, &data(CANCEL_ORDER, accepted.id)).await;
    assert_eq!(
        lines(&refused[0]),
        ["ออเดอร์ IM-20261025-0002 ตอนนี้สถานะ \"ร้านรับออเดอร์แล้ว\"ค่ะ"]
    );
    assert_eq!(
        buttons_of(&refused[0]),
        [
            ("ขอยกเลิก", data(REQUEST_CANCEL, accepted.id).as_str()),
            ("ส่งข้อความถึงร้าน", data(MESSAGE_SHOP, accepted.id).as_str()),
        ]
    );
    assert_eq!(s.get(accepted.id).await, accepted);
}

/// FR-ORD-07: once accepted, the buyer asks to cancel, once; the seller goes on with it, and
/// the buyer is told and may ask again, or cancels it, the request being the reason, and the
/// stock is given back.
#[tokio::test]
async fn the_buyer_asks_to_cancel_and_the_seller_decides() {
    let s = setup().await;
    let accepted = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let id = accepted.id;
    // Nothing to grant or decline yet
    let early = s.taps(s.seller.id, &data(GRANT_CANCEL, id)).await;
    assert_eq!(
        lines(&early[0]),
        ["ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"ร้านรับออเดอร์แล้ว\"ค่ะ"]
    );
    s.taps(s.seller.id, &data(KEEP, id)).await;
    assert_eq!(s.get(id).await, accepted);

    let (asked, essential) = s.tapped(s.buyer.id, &data(REQUEST_CANCEL, id)).await;
    assert!(essential);
    assert_eq!(
        text(&asked[0]),
        "ส่งคำขอยกเลิกออเดอร์ IM-20261025-0001 ถึงร้านแล้วค่ะ รอร้านตอบนะคะ"
    );
    assert_eq!(s.get(id).await.cancel_requested_at, Some(t0()));
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        (pushes[0].0.as_str(), pushes[0].1.as_str()),
        ("order.cancellation_requested", SELLER)
    );
    let told = Pushed::of(&pushes[0].2);
    assert_eq!(told.lines[0], "ลูกค้าขอยกเลิกออเดอร์ IM-20261025-0001ค่ะ");
    assert_eq!(
        told.buttons(),
        [
            ("กำลังไปส่ง", data(DELIVERING, id).as_str()),
            ("ยกเลิกออเดอร์", data(GRANT_CANCEL, id).as_str()),
            ("ไม่ยกเลิก", data(KEEP, id).as_str()),
            ("ส่งข้อความถึงลูกค้า", data(MESSAGE_BUYER, id).as_str()),
        ]
    );
    // Asked once
    let again = s.taps(s.buyer.id, &data(REQUEST_CANCEL, id)).await;
    assert_eq!(
        text(&again[0]),
        "ขอยกเลิกออเดอร์ IM-20261025-0001 ไปแล้วค่ะ รอร้านตอบนะคะ"
    );
    assert_eq!(s.pushes().await.len(), 1);

    // The seller goes on with it
    let kept = s.taps(s.seller.id, &data(KEEP, id)).await;
    assert_eq!(lines(&kept[0]), ["ทำออเดอร์ IM-20261025-0001 ต่อค่ะ"]);
    let order = s.get(id).await;
    assert_eq!(
        (order.status, order.cancel_requested_at),
        (Status::Accepted, None)
    );
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 2);
    let told = Pushed::of(&pushes[1].2);
    assert_eq!(
        told.lines,
        ["ร้านป้าแดงขอทำออเดอร์ IM-20261025-0001 ต่อ ยังไม่ยกเลิกให้ค่ะ"]
    );
    assert_eq!(
        told.buttons(),
        [
            ("ขอยกเลิก", data(REQUEST_CANCEL, id).as_str()),
            ("ส่งข้อความถึงร้าน", data(MESSAGE_SHOP, id).as_str()),
        ]
    );

    // Asked again, and granted
    s.taps(s.buyer.id, &data(REQUEST_CANCEL, id)).await;
    let granted = s.taps(s.seller.id, &data(GRANT_CANCEL, id)).await;
    assert_eq!(
        lines(&granted[0]),
        [
            "ยกเลิกออเดอร์ IM-20261025-0001 แล้วค่ะ",
            "คืนจำนวนสินค้าเข้าสต็อกแล้วค่ะ",
        ]
    );
    let order = s.get(id).await;
    assert_eq!(order.status, Status::Cancelled);
    assert_eq!(order.cancel_reason.as_deref(), Some("ลูกค้าขอยกเลิก"));
    assert_eq!(s.stock().await, Some(8));
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 4);
    let told = Pushed::of(&pushes[3].2);
    assert_eq!(
        told.lines,
        [
            "ร้านป้าแดงยกเลิกออเดอร์ IM-20261025-0001ค่ะ",
            "เหตุผล: ลูกค้าขอยกเลิก",
            "เลือกซื้อสินค้าอื่นได้เลยค่ะ",
        ]
    );
    assert_eq!(
        s.history(id).await,
        [(
            Some(Status::Accepted),
            Status::Cancelled,
            ActorType::Seller,
            Some("ลูกค้าขอยกเลิก".to_owned())
        )]
    );
    // Nothing to ask once cancelled
    let late = s.taps(s.buyer.id, &data(REQUEST_CANCEL, id)).await;
    assert_eq!(buttons_of(&late[0]), []);
}

/// FR-ORD-04, Table 4.6-B: the seller cancels an accepted order for a reason, the buyer is told,
/// and a paid one is to be refunded (FR-PAY-09).
#[tokio::test]
async fn the_seller_cancels_an_accepted_order_for_a_reason() {
    let s = setup().await;
    let accepted = s.order(Status::Ready, order::Fulfillment::Pickup).await;
    let mut row: order::ActiveModel = accepted.clone().into();
    row.payment_status = Set(PaymentStatus::Paid);
    row.update(&s.db).await.unwrap();
    let asked = s.taps(s.seller.id, &data(SELLER_CANCEL, accepted.id)).await;
    assert_eq!(
        text(&asked[0]),
        "ยกเลิกออเดอร์ IM-20261025-0001 เพราะอะไรคะ เลือกด้านล่างได้เลย ลูกค้าจะเห็นเหตุผลนี้ค่ะ"
    );
    s.types(s.seller.id, "ร้านปิดแล้ว").await;
    let order = s.get(accepted.id).await;
    assert_eq!(order.status, Status::Cancelled);
    assert_eq!(order.payment_status, PaymentStatus::RefundDue);
    let pushes = s.pushes().await;
    let told = Pushed::of(&pushes[0].2);
    assert_eq!(
        told.lines,
        [
            "ร้านป้าแดงยกเลิกออเดอร์ IM-20261025-0001ค่ะ",
            "เหตุผล: ร้านปิดแล้ว",
            "คุณชำระเงินไปแล้ว ร้านป้าแดงจะคืนเงินให้ ตกลงกับร้านได้โดยตรงค่ะ",
            "เลือกซื้อสินค้าอื่นได้เลยค่ะ",
        ]
    );
}

/// FR-ORD-09: the buyer confirms receipt of an order ready or on its way, which completes it;
/// before that, it is shown as it is.
#[tokio::test]
async fn the_buyer_confirms_receipt() {
    let s = setup().await;
    let accepted = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let early = s.taps(s.buyer.id, &data(RECEIVED, accepted.id)).await;
    assert_eq!(s.get(accepted.id).await, accepted);
    assert_eq!(
        buttons_of(&early[0]),
        [
            ("ขอยกเลิก", data(REQUEST_CANCEL, accepted.id).as_str()),
            ("ส่งข้อความถึงร้าน", data(MESSAGE_SHOP, accepted.id).as_str()),
        ]
    );

    let delivering = s
        .order(Status::Delivering, order::Fulfillment::Delivery)
        .await;
    let (received, essential) = s.tapped(s.buyer.id, &data(RECEIVED, delivering.id)).await;
    assert!(essential);
    assert_eq!(
        lines(&received[0]),
        ["ออเดอร์ IM-20261025-0002 สำเร็จแล้ว ขอบคุณที่แจ้งค่ะ"]
    );
    let order = s.get(delivering.id).await;
    assert_eq!(
        (order.status, order.completed_at),
        (Status::Completed, Some(t0()))
    );
    assert_eq!(
        s.history(delivering.id).await,
        [(
            Some(Status::Delivering),
            Status::Completed,
            ActorType::Buyer,
            None
        )]
    );
    // Not paid, so the seller is asked whether they got the money (FR-PAY-07)
    let asked = s.templates.ask_paid(
        &order.order_number,
        order.total,
        true,
        vec![
            Postback::new(MARK_PAID)
                .with_int(ORDER_ID, order.id)
                .button(labels::MARK_PAID.clone()),
            Postback::new(NOT_PAID)
                .with_int(ORDER_ID, order.id)
                .button(labels::NOT_PAID.clone()),
        ],
    );
    assert_eq!(
        lines(&asked),
        [
            "ลูกค้าแจ้งว่าได้รับออเดอร์ IM-20261025-0002 แล้วค่ะ",
            "ได้รับเงิน ฿230 แล้วหรือยังคะ",
        ]
    );
    assert_eq!(
        s.pushes().await,
        [push("payment.to_confirm", SELLER, asked)]
    );

    // Paid already, there is nothing to ask
    s.forget_pushes().await;
    let paid = s.order(Status::Ready, order::Fulfillment::Pickup).await;
    let paid = s
        .paid_by(&paid, PaymentMethod::PromptPay, PaymentStatus::Paid)
        .await;
    s.taps(s.buyer.id, &data(RECEIVED, paid.id)).await;
    assert_eq!(s.get(paid.id).await.status, Status::Completed);
    assert_eq!(s.pushes().await, []);
}

/// FR-ORD-12, FR-ADM-08: an admin cancels an open order for a reason, both parties are told,
/// and it is audited; a final order can't be.
#[tokio::test]
async fn an_admin_cancels_an_open_order_for_a_reason() {
    let s = setup().await;
    let accepted = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let asked = s.taps(s.admin.id, &data(ADMIN_CANCEL, accepted.id)).await;
    assert_eq!(
        text(&asked[0]),
        "ยกเลิกออเดอร์ IM-20261025-0001 เพราะอะไรคะ เลือกด้านล่างได้เลย ทั้งลูกค้าและร้านจะเห็นเหตุผลนี้ค่ะ"
    );
    let cancelled = s.types(s.admin.id, "อื่นๆ").await;
    assert_eq!(s.get(accepted.id).await, accepted, "{cancelled:?}");
    s.types(s.admin.id, "ร้านไม่ส่งของ").await;
    let order = s.get(accepted.id).await;
    assert_eq!(order.status, Status::Cancelled);
    assert_eq!(s.stock().await, Some(8));
    let carry_on = vec![cart::continue_shopping()];
    let number = &order.order_number;
    let to_buyer = s.templates.order_cancelled_notice(
        &s.shop.name,
        number,
        true,
        "ร้านไม่ส่งของ",
        false,
        carry_on,
    );
    let to_seller =
        s.templates
            .order_cancelled_by_admin_notice(number, "ร้านไม่ส่งของ", false, Vec::new());
    assert_eq!(
        lines(&to_seller),
        [
            "ผู้ดูแลยกเลิกออเดอร์ IM-20261025-0001ค่ะ",
            "เหตุผล: ร้านไม่ส่งของ",
            "คืนจำนวนสินค้าเข้าสต็อกแล้วค่ะ",
        ]
    );
    assert_eq!(
        s.pushes().await,
        [
            push("order.status", BUYER, to_buyer),
            push("order.cancelled", SELLER, to_seller),
        ]
    );
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin.id),
            "order.cancelled".to_owned(),
            "order".to_owned(),
            Some(order.id),
            serde_json::json!({ "order_number": number, "reason": "ร้านไม่ส่งของ" }),
        )]
    );
    assert_eq!(
        s.history(order.id).await,
        [(
            Some(Status::Accepted),
            Status::Cancelled,
            ActorType::Admin,
            Some("ร้านไม่ส่งของ".to_owned())
        )]
    );

    let completed = s.order(Status::Completed, order::Fulfillment::Pickup).await;
    let refused = s.taps(s.admin.id, &data(ADMIN_CANCEL, completed.id)).await;
    assert_eq!(
        lines(&refused[0]),
        ["ออเดอร์ IM-20261025-0002 ตอนนี้สถานะ \"สำเร็จ\"ค่ะ"]
    );
    assert!(!s.in_flow(s.admin.id).await);
}

/// NFR-SEC-02, FR-ADM-04: only the order's seller, its buyer and the admins act on it, each as
/// themselves; a suspended user can't; an order or ID that isn't there is not found.
#[tokio::test]
async fn only_the_orders_parties_may_act_on_it() {
    let s = setup().await;
    let placed = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let id = placed.id;
    let refused = |denied| vec![s.templates.refused(denied, None)];
    for (user, action) in [
        (s.other_seller, ACCEPT),
        (s.other_seller, REJECT),
        (s.buyer.id, ACCEPT),
        (s.other_buyer, CANCEL_ORDER),
        (s.seller.id, CANCEL_ORDER),
        (s.seller.id, ADMIN_CANCEL),
        (s.buyer.id, ADMIN_CANCEL),
    ] {
        assert_eq!(
            s.taps(user, &data(action, id)).await,
            refused(Denied::NotAllowed),
            "{action} by {user}"
        );
    }
    assert_eq!(
        s.taps(s.seller.id, &data(ACCEPT, id + 1_000)).await,
        refused(Denied::NotFound)
    );
    assert_eq!(
        s.taps(s.seller.id, "order.accept").await,
        refused(Denied::NotFound)
    );
    assert_eq!(
        s.taps(s.seller.id, &data("order.unknown", id)).await,
        refused(Denied::NotFound)
    );

    let suspended = user::ActiveModel {
        id: Unchanged(s.buyer.id),
        status: Set(user::Status::Suspended),
        suspension_reason: Set(Some("หลอกลวง".to_owned())),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();
    assert_eq!(
        s.taps(s.buyer.id, &data(CANCEL_ORDER, id)).await,
        [s.templates.refused(Denied::Suspended, Some("หลอกลวง"))]
    );
    assert_eq!(s.get(id).await, placed);
    assert_eq!(s.history(id).await, []);
    assert_eq!(s.pushes().await, []);
}

/// FR-ORD-08, Table 4.8-A: with `notification.orderStatusToBuyer` off, the buyer is told of no
/// move, while the seller is still told of a cancellation.
#[tokio::test]
async fn status_pushes_to_the_buyer_can_be_turned_off() {
    let s = setup().await;
    s.switches.set(NotificationConfig {
        order_status_to_buyer: false,
        ..s.config.notification
    });
    let placed = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    s.taps(s.seller.id, &data(ACCEPT, placed.id)).await;
    s.taps(s.seller.id, &data(READY, placed.id)).await;
    assert_eq!(s.get(placed.id).await.status, Status::Ready);
    assert_eq!(s.pushes().await, []);

    let other = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    s.taps(s.buyer.id, &data(CANCEL_ORDER, other.id)).await;
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].0, "order.cancelled");
}

/// FR-ADM-04, FR-ORD-12: an order an admin finds by its number shows its status, shop, buyer
/// and total, with "ยกเลิกออเดอร์" while it is open.
#[tokio::test]
async fn an_order_found_by_its_number_offers_cancelling_while_open() {
    let s = setup().await;
    let open = s
        .order(Status::Delivering, order::Fulfillment::Delivery)
        .await;
    let card = found(&s.db, &s.templates, &open).await.unwrap();
    assert_eq!(
        lines(&card),
        [
            "ออเดอร์ IM-20261025-0001",
            "สถานะ: กำลังไปส่ง",
            "ร้านป้าแดง",
            "ลูกค้า: สมชาย",
            "รวม ฿230 · เงินสดตอนรับของ · ยังไม่ชำระ",
            "สั่งเมื่อ 22 ต.ค. 2569 14:30",
        ]
    );
    assert_eq!(
        buttons_of(&card),
        [("ยกเลิกออเดอร์", data(ADMIN_CANCEL, open.id).as_str())]
    );
    let expired = s.order(Status::Expired, order::Fulfillment::Pickup).await;
    let card = found(&s.db, &s.templates, &expired).await.unwrap();
    assert_eq!(buttons_of(&card), []);
    // One handed over unpaid may be recorded paid (FR-PAY-08)
    let owed = s.order(Status::Completed, order::Fulfillment::Pickup).await;
    let card = found(&s.db, &s.templates, &owed).await.unwrap();
    assert_eq!(
        buttons_of(&card),
        [("ได้รับเงินแล้ว", data(MARK_PAID, owed.id).as_str())]
    );
}

/// FR-PAY-03: once the seller accepts an order paid by PromptPay, the buyer is told the shop's
/// PromptPay ID, its name and the exact amount, even with `notification.orderStatusToBuyer` off,
/// as they can't pay without it; if the shop took its ID away since, to ask the shop for it.
/// While they are yet to pay, the order's card shows the ID too. FR-PAY-04: the PromptPay QR
/// code for that amount comes right under, in the same push.
#[tokio::test]
async fn a_promptpay_order_accepted_says_what_to_transfer_where() {
    let s = setup().await;
    s.switches.set(NotificationConfig {
        order_status_to_buyer: false,
        ..s.config.notification
    });
    s.set_promptpay(Some("0812345678")).await;
    let placed = s.order(Status::Pending, order::Fulfillment::Delivery).await;
    let placed = s
        .paid_by(&placed, PaymentMethod::PromptPay, PaymentStatus::Unpaid)
        .await;
    s.taps(s.seller.id, &data(ACCEPT, placed.id)).await;
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        (pushes[0].0.as_str(), pushes[0].1.as_str()),
        ("payment.promptpay", BUYER)
    );
    let told = Pushed::of(&pushes[0].2);
    assert_eq!(
        told.lines,
        [
            "ร้านป้าแดงรับออเดอร์ IM-20261025-0001 แล้วค่ะ กำลังเตรียมของ",
            "โอนพร้อมเพย์ ฿230",
            "เลขพร้อมเพย์: 081-234-5678",
            "ชื่อร้าน: ร้านป้าแดง",
            "โอนแล้วกด \"ส่งสลิป\" แล้วส่งรูปสลิปในแชตนี้ค่ะ",
        ]
    );
    assert_eq!(
        told.buttons(),
        [
            ("ส่งสลิป", data(SEND_SLIP, placed.id).as_str()),
            ("ขอยกเลิก", data(REQUEST_CANCEL, placed.id).as_str()),
            ("ส่งข้อความถึงร้าน", data(MESSAGE_SHOP, placed.id).as_str()),
        ]
    );
    let card = s.details(s.buyer.id, placed.id).await;
    assert!(card.contains("081-234-5678"), "{card}");
    let sent = pushes[0].2.as_ref().unwrap().as_array().unwrap();
    assert_eq!(sent.len(), 2);
    let [qr] = &s.qr_codes(placed.id).await[..] else {
        panic!("one QR code");
    };
    let link = images::product_image_url(&s.config.app.base_url, qr.id).to_string();
    assert_eq!(
        sent[1],
        serde_json::json!({
            "type": "image",
            "originalContentUrl": link,
            "previewImageUrl": link,
        })
    );
    assert_eq!(qr.content_type, "image/png");
    let paying = payments::promptpay_payload("0812345678", Some(Money::from_baht(230)));
    assert_eq!(Some(payments::scan_qr(&qr.data)), paying);

    s.forget_pushes().await;
    s.set_promptpay(None).await;
    let other = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    let other = s
        .paid_by(&other, PaymentMethod::PromptPay, PaymentStatus::Unpaid)
        .await;
    s.taps(s.seller.id, &data(ACCEPT, other.id)).await;
    let pushes = s.pushes().await;
    assert_eq!(
        Pushed::of(&pushes[0].2).lines[1..],
        [
            "โอนพร้อมเพย์ ฿220",
            "ร้านยังไม่มีเลขพร้อมเพย์ สอบถามเลขที่จะโอนกับร้านได้โดยตรงค่ะ",
        ]
    );
    // Nothing to draw a QR code of
    assert_eq!(pushes[0].2.as_ref().unwrap().as_array().unwrap().len(), 1);
    assert_eq!(s.qr_codes(other.id).await, []);

    // A cash order's acceptance still follows the switch
    s.forget_pushes().await;
    let cash = s.order(Status::Pending, order::Fulfillment::Pickup).await;
    s.taps(s.seller.id, &data(ACCEPT, cash.id)).await;
    assert_eq!(s.pushes().await, []);
    assert_eq!(s.qr_codes(cash.id).await, []);
}

/// FR-PAY-07: asked whether they got the money of an order handed over, the seller answers
/// "ยังไม่ได้รับ", which leaves it unpaid under "ค้างชำระ", then records it with "ได้รับเงินแล้ว"
/// once they do; the buyer is told. Tapped again, it shows the order as it is.
#[tokio::test]
async fn the_seller_answers_whether_they_got_the_money() {
    let s = setup().await;
    let placed = s
        .order(Status::Delivering, order::Fulfillment::Delivery)
        .await;
    let id = placed.id;
    s.taps(s.seller.id, &data(COMPLETE, id)).await;
    s.forget_pushes().await;

    let not_yet = s.taps(s.seller.id, &data(NOT_PAID, id)).await;
    assert_eq!(
        lines(&not_yet[0]),
        [
            "ออเดอร์ IM-20261025-0001 ยังค้างชำระค่ะ",
            "ดูได้ที่ \"ออเดอร์ร้าน\" > \"ค้างชำระ\" ได้รับเงินแล้วกด \"ได้รับเงินแล้ว\"ค่ะ",
        ]
    );
    assert_eq!(
        buttons_of(&not_yet[0]),
        [("ได้รับเงินแล้ว", data(MARK_PAID, id).as_str())]
    );
    let order = s.get(id).await;
    assert_eq!(order.payment_status, PaymentStatus::Unpaid);
    assert_eq!(s.pushes().await, []);

    let (paid, essential) = s.tapped(s.seller.id, &data(MARK_PAID, id)).await;
    assert!(essential);
    assert_eq!(
        lines(&paid[0]),
        ["บันทึกว่าได้รับเงินออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    assert_eq!(buttons_of(&paid[0]), []);
    assert_eq!(s.get(id).await.payment_status, PaymentStatus::Paid);
    // The order is completed, so the buyer may rate it (FR-ORD-14)
    let told = s.templates.payment_recorded_notice(
        &s.shop.name,
        &order.order_number,
        PaymentStatus::Paid,
        false,
        vec![rating::button(&order)],
    );
    assert_eq!(
        lines(&told),
        ["ร้านป้าแดงยืนยันว่าได้รับเงินค่าออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    assert_eq!(s.pushes().await, [push("order.status", BUYER, told)]);

    for action in [MARK_PAID, NOT_PAID] {
        let again = s.taps(s.seller.id, &data(action, id)).await;
        assert_eq!(
            lines(&again[0]),
            [
                "ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"สำเร็จ\"ค่ะ",
                "การชำระเงิน: ชำระแล้ว",
            ]
        );
    }
    assert_eq!(s.pushes().await.len(), 1);
    assert_eq!(s.audited().await, []);
}

/// FR-PAY-03, FR-PAY-08: the seller records a transfer as soon as it arrives, from the order's
/// card, while cash waits for the hand-over, as in Appendix A.4.
#[tokio::test]
async fn a_transfer_is_recorded_before_the_hand_over() {
    let s = setup().await;
    let cash = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let early = s.taps(s.seller.id, &data(MARK_PAID, cash.id)).await;
    assert_eq!(s.get(cash.id).await, cash);
    assert_eq!(
        buttons_of(&early[0]),
        [
            ("กำลังไปส่ง", data(DELIVERING, cash.id).as_str()),
            ("ยกเลิกออเดอร์", data(SELLER_CANCEL, cash.id).as_str()),
            ("ส่งข้อความถึงลูกค้า", data(MESSAGE_BUYER, cash.id).as_str()),
        ]
    );

    let transfer = s
        .order(Status::Accepted, order::Fulfillment::Delivery)
        .await;
    let transfer = s
        .paid_by(&transfer, PaymentMethod::PromptPay, PaymentStatus::Unpaid)
        .await;
    let id = transfer.id;
    let card = s.details(s.seller.id, id).await;
    assert!(card.contains(&data(MARK_PAID, id)), "{card}");
    let paid = s.taps(s.seller.id, &data(MARK_PAID, id)).await;
    assert_eq!(
        buttons_of(&paid[0]),
        [
            ("กำลังไปส่ง", data(DELIVERING, id).as_str()),
            ("ยกเลิกออเดอร์", data(SELLER_CANCEL, id).as_str()),
            ("ส่งข้อความถึงลูกค้า", data(MESSAGE_BUYER, id).as_str()),
        ]
    );
    let order = s.get(id).await;
    assert_eq!(
        (order.status, order.payment_status),
        (Status::Accepted, PaymentStatus::Paid)
    );

    // Handed over paid, there is nothing to ask
    let handed = s.taps(s.seller.id, &data(DELIVERING, id)).await;
    assert_eq!(buttons_of(&handed[0])[0].0, "ส่งมอบแล้ว");
    let handed = s.taps(s.seller.id, &data(COMPLETE, id)).await;
    assert_eq!(lines(&handed[0]), ["ออเดอร์ IM-20261025-0002 สำเร็จแล้วค่ะ"]);
}

/// The acceptance criterion of #35, FR-PAY-08: a buyer can't record their own order paid,
/// refunded or anything, not by its buttons, and not as an admin either. Nothing changes.
#[tokio::test]
async fn a_buyer_cannot_set_their_own_order_paid() {
    let s = setup().await;
    let owed = s.order(Status::Completed, order::Fulfillment::Pickup).await;
    let refund = s.order(Status::Cancelled, order::Fulfillment::Pickup).await;
    let refund = s
        .paid_by(&refund, PaymentMethod::Cash, PaymentStatus::RefundDue)
        .await;
    let refused = vec![s.templates.refused(Denied::NotAllowed, None)];
    for (user, action, order) in [
        (s.buyer.id, MARK_PAID, &owed),
        (s.buyer.id, NOT_PAID, &owed),
        (s.buyer.id, MARK_REFUNDED, &refund),
        (s.other_buyer, MARK_PAID, &owed),
        (s.other_seller, MARK_PAID, &owed),
        (s.other_seller, MARK_REFUNDED, &refund),
        (s.admin.id, NOT_PAID, &owed),
    ] {
        assert_eq!(
            s.taps(user, &data(action, order.id)).await,
            refused,
            "{action} by {user}"
        );
        assert_eq!(&s.get(order.id).await, order);
    }

    // An admin who bought it is its buyer
    let own = order::ActiveModel {
        buyer_id: Set(s.admin.id),
        ..s.order(Status::Completed, order::Fulfillment::Pickup)
            .await
            .into()
    };
    let own = own.update(&s.db).await.unwrap();
    let shown = s.taps(s.admin.id, &data(MARK_PAID, own.id)).await;
    assert_eq!(
        lines(&shown[0]),
        [
            "ออเดอร์ IM-20261025-0003 ตอนนี้สถานะ \"สำเร็จ\"ค่ะ",
            "การชำระเงิน: ยังไม่ชำระ",
        ]
    );
    assert_eq!(s.get(own.id).await, own);
    assert_eq!(s.pushes().await, []);
    assert_eq!(s.audited().await, []);
}

/// The acceptance criterion of #35, FR-PAY-09: cancelling a paid order, as its seller or an
/// admin, leaves it to be refunded; the seller is told to refund the buyer, and records it with
/// "คืนเงินแล้ว", from the reply, the admin's notice or "ค้างชำระ"; the buyer is told.
#[tokio::test]
async fn a_paid_order_cancelled_is_refunded_and_recorded() {
    let s = setup().await;
    let placed = s.order(Status::Ready, order::Fulfillment::Pickup).await;
    let placed = s
        .paid_by(&placed, PaymentMethod::PromptPay, PaymentStatus::Paid)
        .await;
    let id = placed.id;
    s.taps(s.seller.id, &data(SELLER_CANCEL, id)).await;
    let cancelled = s.types(s.seller.id, "ร้านปิดแล้ว").await;
    assert_eq!(s.get(id).await.payment_status, PaymentStatus::RefundDue);
    assert_eq!(
        lines(&cancelled[0]),
        [
            "ยกเลิกออเดอร์ IM-20261025-0001 แล้วค่ะ",
            "คืนจำนวนสินค้าเข้าสต็อกแล้วค่ะ",
            "ลูกค้าชำระเงินแล้ว กรุณาคืนเงินให้ลูกค้า แล้วกด \"คืนเงินแล้ว\"ค่ะ",
        ]
    );
    assert_eq!(
        buttons_of(&cancelled[0]),
        [("คืนเงินแล้ว", data(MARK_REFUNDED, id).as_str())]
    );
    s.forget_pushes().await;
    let refunded = s.taps(s.seller.id, &data(MARK_REFUNDED, id)).await;
    assert_eq!(
        lines(&refunded[0]),
        ["บันทึกว่าคืนเงินออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    assert_eq!(buttons_of(&refunded[0]), []);
    let order = s.get(id).await;
    assert_eq!(
        (order.status, order.payment_status),
        (Status::Cancelled, PaymentStatus::Refunded)
    );
    let pushes = s.pushes().await;
    assert_eq!(
        Pushed::of(&pushes[0].2).lines,
        ["ร้านป้าแดงแจ้งว่าคืนเงินค่าออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );

    // Cancelled by an admin, the seller's notice records the refund
    let placed = s
        .order(Status::Delivering, order::Fulfillment::Delivery)
        .await;
    let placed = s
        .paid_by(&placed, PaymentMethod::Cash, PaymentStatus::Paid)
        .await;
    s.forget_pushes().await;
    s.taps(s.admin.id, &data(ADMIN_CANCEL, placed.id)).await;
    s.types(s.admin.id, "ร้านไม่ส่งของ").await;
    assert_eq!(
        s.get(placed.id).await.payment_status,
        PaymentStatus::RefundDue
    );
    let pushes = s.pushes().await;
    let to_seller = Pushed::of(&pushes[1].2);
    assert_eq!(pushes[1].1, SELLER);
    assert_eq!(
        to_seller.lines[3],
        "ลูกค้าชำระเงินแล้ว กรุณาคืนเงินให้ลูกค้า แล้วกด \"คืนเงินแล้ว\"ค่ะ"
    );
    assert_eq!(
        to_seller.buttons(),
        [("คืนเงินแล้ว", data(MARK_REFUNDED, placed.id).as_str())]
    );
}

/// FR-PAY-08, FR-ADM-08: an admin records a payment and a refund from the order's card in
/// "ผู้ใช้"; each is audited, and the buyer is told it was an admin.
#[tokio::test]
async fn an_admin_records_a_payment_and_it_is_audited() {
    let s = setup().await;
    let owed = s.order(Status::Completed, order::Fulfillment::Pickup).await;
    let recorded = s.taps(s.admin.id, &data(MARK_PAID, owed.id)).await;
    assert_eq!(
        lines(&recorded[0]),
        ["บันทึกว่าได้รับเงินออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    assert_eq!(s.get(owed.id).await.payment_status, PaymentStatus::Paid);
    let pushes = s.pushes().await;
    assert_eq!(
        Pushed::of(&pushes[0].2).lines,
        ["ผู้ดูแลยืนยันว่าได้รับเงินค่าออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );

    let refund = s.order(Status::Cancelled, order::Fulfillment::Pickup).await;
    let refund = s
        .paid_by(&refund, PaymentMethod::Cash, PaymentStatus::RefundDue)
        .await;
    s.taps(s.admin.id, &data(MARK_REFUNDED, refund.id)).await;
    assert_eq!(
        s.get(refund.id).await.payment_status,
        PaymentStatus::Refunded
    );
    let audit = |action: &str, order: &order::Model| {
        (
            Some(s.admin.id),
            action.to_owned(),
            "order".to_owned(),
            Some(order.id),
            serde_json::json!({ "order_number": order.order_number }),
        )
    };
    assert_eq!(
        s.audited().await,
        [
            audit("payment.confirmed", &owed),
            audit("payment.refunded", &refund)
        ]
    );
}
