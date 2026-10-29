//! Relaying messages (FR-ORD-13): while an order is active, its buyer taps "ส่งข้อความถึงร้าน"
//! and its seller "ส่งข้อความถึงลูกค้า", types the message, and the other party gets it by push,
//! led by the order's number, with the button to answer back. Nothing is relayed for an order
//! that isn't active, by anyone but its parties, or to a party who doesn't follow the OA. Each
//! event is handled in a transaction committed once it is handled, with what it notifies queued
//! in it. The tests each start a throwaway PostgreSQL, so they need Docker.

use std::cell::Cell;
use std::slice;

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{ActiveModelTrait, DatabaseConnection, QueryOrder, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, Config, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::Templates;
use crate::notifications::templates::order_status_name;
use crate::orders::{READY, REQUEST_CANCEL, SELLER_CANCEL};
use crate::persistence::entity::order::{Fulfillment, Status};
use crate::persistence::entity::{push_log, session, shop};
use crate::persistence::{orm, random_id, testing};
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
    templates: Templates,
    seller: i64,
    buyer: i64,
    admin: i64,
    other_seller: i64,
    other_buyer: i64,
    shop: shop::Model,
    orders: Cell<u32>,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let buyer = user::ActiveModel {
        display_name: Set(Some("สมชาย".to_owned())),
        ..testing::new_user(BUYER)
    };
    let buyer = buyer.insert(&db).await.unwrap().id;
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(ADMIN)
    };
    let admin = admin.insert(&db).await.unwrap().id;
    let other_seller = testing::new_user(OTHER_SELLER)
        .insert(&db)
        .await
        .unwrap()
        .id;
    testing::new_shop(other_seller).insert(&db).await.unwrap();
    let other_buyer = testing::new_user(OTHER_BUYER).insert(&db).await.unwrap().id;
    let shop = shop::ActiveModel {
        name: Set("ร้านป้าแดง".to_owned()),
        ..testing::new_shop(seller)
    };
    let shop = shop.insert(&db).await.unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        seller,
        buyer,
        admin,
        other_seller,
        other_buyer,
        shop,
        orders: Cell::new(0),
    }
}

impl Setup {
    /// An order of the buyer's at the shop, `status`, picked up or delivered as `fulfillment`
    /// says, and paid in cash. The first is IM-20261025-0001.
    async fn order(&self, status: Status, fulfillment: Fulfillment) -> order::Model {
        self.orders.set(self.orders.get() + 1);
        let number = format!("IM-20261025-{:04}", self.orders.get());
        order::ActiveModel {
            status: Set(status),
            fulfillment: Set(fulfillment),
            ..testing::new_order(self.buyer, self.shop.id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap()
    }

    /// An order of the buyer's the shop accepted, to be picked up.
    async fn accepted(&self) -> order::Model {
        self.order(Status::Accepted, Fulfillment::Pickup).await
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
            .queue(&txn, &self.config.notification, t0())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        (messages, essential)
    }

    /// User `user_id` taps a button sending `data`.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            actions::tapped(turn, &self.config, &postback).await
        })
        .await
        .0
    }

    /// User `user_id` answers the step of sending a message they are on with `input`; `None`
    /// if the step doesn't take it.
    async fn sends(&self, user_id: i64, input: Input) -> (Option<Vec<Message>>, bool) {
        let mut answered = None;
        let (_, essential) = self
            .handle(user_id, async |turn| {
                let session = turn.flow.as_ref().and_then(|flow| flow.read::<RelayFlow>());
                let session = session.expect("sending a message");
                answered = answer(turn, session, &input).await?;
                Ok(answered.clone().unwrap_or_default())
            })
            .await;
        (answered, essential)
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(user_id, input).await.0.expect("an answer")
    }

    /// The step of sending a message the user is on, if any.
    async fn step(&self, user_id: i64) -> Option<(Step, RelayFlow)> {
        let row = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()?;
        assert_eq!(row.flow, RelayFlow::NAME);
        let step = serde_json::from_value(Json::String(row.step)).unwrap();
        Some((step, serde_json::from_value(row.data).unwrap()))
    }

    /// Order `id` moves to `status`, as by a tap of its seller's meanwhile.
    async fn moves(&self, id: i64, status: Status) {
        order::ActiveModel {
            id: Unchanged(id),
            status: Set(status),
            ..Default::default()
        }
        .update(&self.db)
        .await
        .unwrap();
    }

    /// User `user_id` follows the OA, or blocked it.
    async fn follows(&self, user_id: i64, following: bool) {
        user::ActiveModel {
            id: Unchanged(user_id),
            is_following: Set(following),
            ..Default::default()
        }
        .update(&self.db)
        .await
        .unwrap();
    }

    /// Every push queued, as (type, recipient, messages), oldest first.
    async fn pushes(&self) -> Vec<(String, String, Json)> {
        push_log::Entity::find()
            .order_by_asc(push_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.push_type, row.recipient, row.messages.unwrap()))
            .collect()
    }

    /// The reply refusing what the actor asked, as `denied`.
    fn refused(&self, denied: Denied) -> Message {
        self.templates.refused(denied, None)
    }
}

/// The postback data of `action` on order `id`.
fn data(action: &str, id: i64) -> String {
    format!("{action}?order={id}")
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

/// The preview, lines and buttons of the notice pushed as `messages[0]`, as LINE gets it.
fn pushed(messages: &Json) -> (String, Vec<String>, Vec<(String, String)>) {
    let preview = messages[0]["altText"].as_str().unwrap().to_owned();
    let card = &messages[0]["contents"];
    let texts = card["body"]["contents"].as_array().unwrap().iter();
    let lines = texts.map(|text| text["text"].as_str().unwrap().to_owned());
    let footer = card["footer"]["contents"].as_array();
    let buttons = footer.into_iter().flatten().map(|button| {
        let action = &button["action"];
        let field = |name: &str| action[name].as_str().unwrap().to_owned();
        (field("label"), field("data"))
    });
    (preview, lines.collect(), buttons.collect())
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

/// Whether the buttons of a notice offer to send a message, as either party.
fn offers_relay(message: &Message) -> bool {
    let relay = |data: &str| data.starts_with(MESSAGE_SHOP) || data.starts_with(MESSAGE_BUYER);
    buttons_of(message).into_iter().any(|(_, data)| relay(data))
}

/// Whether `buttons` offer to send a message, as either party.
fn relays(buttons: &[Action]) -> bool {
    buttons.iter().any(|action| match action {
        Action::Postback { data, .. } => {
            data.starts_with(MESSAGE_SHOP) || data.starts_with(MESSAGE_BUYER)
        }
        _ => false,
    })
}

/// FR-ORD-13: the buyer taps "ส่งข้อความถึงร้าน" and types; the seller gets it by push, led by
/// the order's number, with "ส่งข้อความถึงลูกค้า" to answer back, which reaches the buyer the
/// same way.
#[tokio::test]
async fn the_buyer_messages_the_shop_and_the_seller_answers_back() {
    let s = setup().await;
    let order = s.accepted().await;
    let id = order.id;

    let asked = s.taps(s.buyer, &data(MESSAGE_SHOP, id)).await;
    assert_eq!(
        text(&asked[0]),
        "พิมพ์ข้อความถึงร้านป้าแดง เรื่องออเดอร์ IM-20261025-0001 ได้เลยค่ะ ไม่เกิน 300 ตัวอักษร ระบบจะส่งต่อให้ทางแชตนี้ค่ะ"
    );
    assert_eq!(quick_replies(&asked[0]), ["ยกเลิก"]);
    let flow = RelayFlow {
        order_id: id,
        from: Side::Buyer,
    };
    assert_eq!(s.step(s.buyer).await, Some((Step::Text, flow)));
    assert!(s.pushes().await.is_empty());

    let (sent, essential) = s
        .sends(s.buyer, Input::Text(" พรุ่งนี้เช้าไปรับได้ไหมคะ ".to_owned()))
        .await;
    let sent = sent.expect("an answer");
    assert!(essential);
    assert_eq!(
        lines(&sent[0]),
        ["ส่งข้อความถึงร้านป้าแดง แล้วค่ะ", "ออเดอร์ IM-20261025-0001"]
    );
    let (cancel, again) = (data(REQUEST_CANCEL, id), data(MESSAGE_SHOP, id));
    assert_eq!(
        buttons_of(&sent[0]),
        [
            ("ขอยกเลิก", cancel.as_str()),
            ("ส่งข้อความถึงร้าน", again.as_str())
        ]
    );
    assert_eq!(s.step(s.buyer).await, None);

    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    let (kind, to, messages) = &pushes[0];
    assert_eq!((kind.as_str(), to.as_str()), ("order.message", SELLER));
    let (preview, told, buttons) = pushed(messages);
    assert_eq!(preview, "IM-20261025-0001 ลูกค้า สมชาย: พรุ่งนี้เช้าไปรับได้ไหมคะ");
    assert_eq!(
        told,
        [
            "IM-20261025-0001 · ข้อความจากลูกค้า สมชาย",
            "พรุ่งนี้เช้าไปรับได้ไหมคะ",
        ]
    );
    assert_eq!(
        buttons,
        [
            ("ส่งข้อความถึงลูกค้า".to_owned(), data(MESSAGE_BUYER, id)),
            ("ดูรายละเอียด".to_owned(), data(VIEW, id)),
        ]
    );

    // The seller answers back
    let asked = s.taps(s.seller, &data(MESSAGE_BUYER, id)).await;
    assert_eq!(
        text(&asked[0]),
        "พิมพ์ข้อความถึงลูกค้า สมชาย เรื่องออเดอร์ IM-20261025-0001 ได้เลยค่ะ ไม่เกิน 300 ตัวอักษร ระบบจะส่งต่อให้ทางแชตนี้ค่ะ"
    );
    let sent = s.types(s.seller, "ได้ค่ะ ร้านเปิด 7 โมง").await;
    assert_eq!(
        lines(&sent[0]),
        ["ส่งข้อความถึงลูกค้า สมชาย แล้วค่ะ", "ออเดอร์ IM-20261025-0001"]
    );
    let (ready, cancel, again) = (
        data(READY, id),
        data(SELLER_CANCEL, id),
        data(MESSAGE_BUYER, id),
    );
    assert_eq!(
        buttons_of(&sent[0]),
        [
            ("พร้อมให้รับ", ready.as_str()),
            ("ยกเลิกออเดอร์", cancel.as_str()),
            ("ส่งข้อความถึงลูกค้า", again.as_str()),
        ]
    );
    assert_eq!(s.step(s.seller).await, None);

    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 2);
    let (kind, to, messages) = &pushes[1];
    assert_eq!((kind.as_str(), to.as_str()), ("order.message", BUYER));
    let (preview, told, buttons) = pushed(messages);
    assert_eq!(preview, "IM-20261025-0001 ร้านป้าแดง: ได้ค่ะ ร้านเปิด 7 โมง");
    assert_eq!(
        told,
        ["IM-20261025-0001 · ข้อความจากร้านป้าแดง", "ได้ค่ะ ร้านเปิด 7 โมง",]
    );
    assert_eq!(
        buttons,
        [
            ("ส่งข้อความถึงร้าน".to_owned(), data(MESSAGE_SHOP, id)),
            ("ดูรายละเอียด".to_owned(), data(VIEW, id)),
        ]
    );
}

/// FR-ORD-13: every active status relays, ready for pickup and on its way alike; the buttons
/// of both parties offer it, last.
#[tokio::test]
async fn an_order_ready_or_on_its_way_relays_too() {
    let s = setup().await;
    for (status, fulfillment) in [
        (Status::Ready, Fulfillment::Pickup),
        (Status::Delivering, Fulfillment::Delivery),
    ] {
        let order = s.order(status, fulfillment).await;
        for (viewer, side) in [
            (ActorType::Buyer, Side::Buyer),
            (ActorType::Seller, Side::Seller),
        ] {
            let buttons = actions::buttons(&order, viewer);
            assert_eq!(buttons.last(), Some(&button(&order, side)), "{status:?}");
        }
        s.taps(s.seller, &data(MESSAGE_BUYER, order.id)).await;
        let sent = s.types(s.seller, "กำลังไปค่ะ").await;
        assert_eq!(lines(&sent[0])[0], "ส่งข้อความถึงลูกค้า สมชาย แล้วค่ะ");
        let (kind, to, messages) = s.pushes().await.pop().unwrap();
        assert_eq!((kind.as_str(), to.as_str()), ("order.message", BUYER));
        assert_eq!(pushed(&messages).1[1], "กำลังไปค่ะ");
    }
    assert_eq!(s.pushes().await.len(), 2);
}

/// FR-ORD-13 acceptance: an order yet to be accepted, or over, relays nothing. Its buttons don't
/// offer it to either party, and an old button shows the order as it is, asking for nothing.
#[tokio::test]
async fn nothing_is_relayed_for_an_order_not_active() {
    let s = setup().await;
    for status in [
        Status::Pending,
        Status::Completed,
        Status::Rejected,
        Status::Cancelled,
        Status::Expired,
    ] {
        let order = s.order(status, Fulfillment::Pickup).await;
        assert!(!relays(&actions::buttons(&order, ActorType::Buyer)));
        assert!(!relays(&actions::buttons(&order, ActorType::Seller)));
        assert!(!relays(&actions::buttons(&order, ActorType::Admin)));

        let (number, name) = (&order.order_number, order_status_name(status));
        let now = format!("ออเดอร์ {number} ตอนนี้สถานะ \"{name}\"ค่ะ");
        for (user_id, action) in [(s.buyer, MESSAGE_SHOP), (s.seller, MESSAGE_BUYER)] {
            let shown = s.taps(user_id, &data(action, order.id)).await;
            assert_eq!(lines(&shown[0]), [now.as_str()], "{status:?}");
            assert!(!offers_relay(&shown[0]), "{status:?}");
            assert_eq!(s.step(user_id).await, None, "{status:?}");
        }
    }
    assert!(s.pushes().await.is_empty());
}

/// FR-UI-13: a message typed while the order moved on, completed or cancelled, isn't sent; the
/// sender sees the order as it is now, and the flow ends.
#[tokio::test]
async fn a_message_typed_as_the_order_moves_on_is_not_sent() {
    let s = setup().await;
    for status in [Status::Completed, Status::Cancelled] {
        let order = s.accepted().await;
        s.taps(s.buyer, &data(MESSAGE_SHOP, order.id)).await;
        s.moves(order.id, status).await;
        let shown = s.types(s.buyer, "ถึงไหนแล้วคะ").await;
        let (number, name) = (&order.order_number, order_status_name(status));
        let now = format!("ออเดอร์ {number} ตอนนี้สถานะ \"{name}\"ค่ะ");
        assert_eq!(lines(&shown[0]), [now.as_str()]);
        assert!(!offers_relay(&shown[0]));
        assert_eq!(s.step(s.buyer).await, None);
    }
    assert!(s.pushes().await.is_empty());
}

/// NFR-SEC-02, NFR-SEC-10: only the order's buyer messages its shop, and only its shop's owner
/// its buyer; a suspended buyer can't, even mid-flow, while the owner of a suspended shop still
/// can, its orders going on (FR-SHOP-10).
#[tokio::test]
async fn only_the_orders_parties_message_each_other() {
    let s = setup().await;
    let order = s.accepted().await;
    let id = order.id;
    let not_allowed = s.refused(Denied::NotAllowed);
    for (user_id, action) in [
        (s.other_buyer, MESSAGE_SHOP),
        (s.other_seller, MESSAGE_BUYER),
        (s.admin, MESSAGE_SHOP),
        (s.admin, MESSAGE_BUYER),
        // Each party only as themselves
        (s.seller, MESSAGE_SHOP),
        (s.buyer, MESSAGE_BUYER),
    ] {
        let refused = s.taps(user_id, &data(action, id)).await;
        assert_eq!(
            refused,
            slice::from_ref(&not_allowed),
            "{action} by {user_id}"
        );
        assert!(
            session::Entity::find_by_id(user_id)
                .one(&s.db)
                .await
                .unwrap()
                .is_none()
        );
    }

    // The buyer is suspended while typing
    s.taps(s.buyer, &data(MESSAGE_SHOP, id)).await;
    let suspended = user::ActiveModel {
        id: Unchanged(s.buyer),
        status: Set(user::Status::Suspended),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();
    let refused = s.types(s.buyer, "ยังส่งได้ไหม").await;
    assert_eq!(refused, [s.refused(Denied::Suspended)]);
    assert_eq!(s.step(s.buyer).await, None);
    let refused = s.taps(s.buyer, &data(MESSAGE_SHOP, id)).await;
    assert_eq!(refused, [s.refused(Denied::Suspended)]);
    assert!(s.pushes().await.is_empty());

    // A suspended shop's orders go on
    let suspended = shop::ActiveModel {
        id: Unchanged(s.shop.id),
        status: Set(shop::Status::Suspended),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();
    s.taps(s.seller, &data(MESSAGE_BUYER, id)).await;
    s.types(s.seller, "ยังส่งของให้ตามเดิมค่ะ").await;
    let (kind, to, _) = s.pushes().await.pop().unwrap();
    assert_eq!((kind.as_str(), to.as_str()), ("order.message", BUYER));
}

/// FR-ORD-13: a message is 1 to 300 characters, without the spaces around it; nothing, or more,
/// is asked for again, and the flow waits on.
#[tokio::test]
async fn a_message_is_short() {
    let s = setup().await;
    let order = s.accepted().await;
    let asked = s.taps(s.buyer, &data(MESSAGE_SHOP, order.id)).await;

    let again = s.types(s.buyer, " \n ").await;
    assert_eq!(text(&again[0]), text(&asked[0]));
    assert_eq!(quick_replies(&again[0]), ["ยกเลิก"]);
    assert_eq!(s.step(s.buyer).await.unwrap().0, Step::Text);

    let longest = "ก".repeat(MESSAGE_CHARS);
    let too_long = s.types(s.buyer, &format!("{longest}ก")).await;
    assert_eq!(
        text(&too_long[0]),
        "ขอโทษค่ะ ข้อความยาวเกินไป พิมพ์ใหม่ให้ไม่เกิน 300 ตัวอักษรได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&too_long[0]), ["ยกเลิก"]);
    assert_eq!(s.step(s.buyer).await.unwrap().0, Step::Text);
    assert!(s.pushes().await.is_empty());

    // Not text: routing goes on
    assert_eq!(s.sends(s.buyer, Input::Unsupported).await.0, None);

    s.types(s.buyer, &format!("  {longest}\n")).await;
    assert_eq!(s.step(s.buyer).await, None);
    let (_, _, messages) = s.pushes().await.pop().unwrap();
    assert_eq!(pushed(&messages).1[1], longest);
}

/// FR-NOT-02: no push reaches a party who doesn't follow the OA, so the sender is told they
/// can't be messaged, with what else they can do, instead of being asked for one; and one who
/// blocks the OA while the message is typed doesn't get it either.
#[tokio::test]
async fn no_message_goes_to_a_party_who_does_not_follow_the_oa() {
    let s = setup().await;
    let order = s.accepted().await;
    let id = order.id;
    let cancel = data(REQUEST_CANCEL, id);

    s.follows(s.seller, false).await;
    let told = s.taps(s.buyer, &data(MESSAGE_SHOP, id)).await;
    assert_eq!(
        lines(&told[0]),
        [
            "ส่งข้อความถึงร้านป้าแดงไม่ได้ค่ะ",
            "ออเดอร์ IM-20261025-0001",
            "ร้านป้าแดงไม่ได้เป็นเพื่อนกับ IconMarket ในไลน์อยู่ จึงส่งข้อความต่อให้ไม่ได้ค่ะ",
        ]
    );
    assert_eq!(buttons_of(&told[0]), [("ขอยกเลิก", cancel.as_str())]);
    assert_eq!(s.step(s.buyer).await, None);

    // Blocked while the message is typed
    s.follows(s.seller, true).await;
    s.taps(s.buyer, &data(MESSAGE_SHOP, id)).await;
    s.follows(s.seller, false).await;
    let told = s.types(s.buyer, "สวัสดีค่ะ").await;
    assert_eq!(lines(&told[0])[0], "ส่งข้อความถึงร้านป้าแดงไม่ได้ค่ะ");
    assert_eq!(s.step(s.buyer).await, None);

    s.follows(s.buyer, false).await;
    let told = s.taps(s.seller, &data(MESSAGE_BUYER, id)).await;
    assert_eq!(lines(&told[0])[0], "ส่งข้อความถึงลูกค้า สมชายไม่ได้ค่ะ");
    assert!(!offers_relay(&told[0]));
    assert!(s.pushes().await.is_empty());
}
