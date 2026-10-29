//! Sending a payment slip (FR-PAY-05, FR-UI-14): the buyer taps "ส่งสลิป" and sends the photo,
//! or sends one unasked and says it is the slip; the seller gets it behind its expiring link
//! (AC-14) with "ได้รับเงินแล้ว" and "ยังไม่ได้รับ"; the first records it paid, the second tells
//! the buyer. Only the order's buyer sends it, and only its seller or an admin confirms it.
//! While `ai.features.slipReading` is on, the seller also sees what the AI read from the slip,
//! as advice (FR-PAY-06). Each
//! event is handled in a transaction committed once it is handled, with what it notifies queued
//! in it. The tests each start a throwaway PostgreSQL, so they need Docker.

use std::cell::Cell;

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, QueryFilter, QueryOrder, TransactionTrait,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::ai::testing::{self as scripted, Model, Turn as Scripted};
use crate::ai::{self, Part};
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer};
use crate::line::testing::{self as fake, Fake};
use crate::line::{Body, limits};
use crate::notifications::templates::Templates;
use crate::orders::{MESSAGE_SHOP, REQUEST_CANCEL, SEND_SLIP, actions};
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::order::{PaymentMethod, PaymentStatus, Status};
use crate::persistence::entity::{
    ai_call_log, audit_log, payment_slip, push_log, session, shop, user,
};
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

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    /// Off, unless a test has [the AI read slips](Setup::ai_reads)
    ai: Gateway,
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
    let db = orm(pool.clone());
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
        promptpay_id: Set(Some("0812345678".to_owned())),
        ..testing::new_shop(seller)
    };
    let shop = shop.insert(&db).await.unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        pool,
        db,
        line: Fake::start().await,
        ai: ai::testing::off(),
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
    /// From now on the AI accepts images and answers with `says` in turn, with every feature on
    /// but those in `off`; the model knows what it was asked.
    fn ai_reads(&mut self, says: impl IntoIterator<Item = Scripted>, off: &[Feature]) -> Model {
        let (ai, model) = scripted::scripted(&self.pool, says, true, off);
        self.ai = ai;
        model
    }

    /// An order of the buyer's, accepted and to be paid by PromptPay, not yet paid.
    async fn order(&self) -> order::Model {
        self.order_paid_by(Status::Accepted, PaymentMethod::PromptPay)
            .await
    }

    async fn order_paid_by(&self, status: Status, method: PaymentMethod) -> order::Model {
        self.orders.set(self.orders.get() + 1);
        let number = format!("IM-20261025-{:04}", self.orders.get());
        order::ActiveModel {
            status: Set(status),
            payment_method: Set(method),
            ..testing::new_order(self.buyer, self.shop.id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap()
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
    async fn tapped(&self, user_id: i64, data: &str) -> (Vec<Message>, bool) {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            actions::tapped(turn, &self.config, &postback).await
        })
        .await
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.tapped(user_id, data).await.0
    }

    /// LINE serves the photo as the content of message `message_id`.
    fn serves(&self, message_id: &str) {
        let path = format!("/v2/bot/message/{message_id}/content");
        self.line
            .script(&path, [fake::Answer::bytes("image/jpeg", PHOTO)]);
    }

    /// User `user_id` answers the step of sending a slip they are on with `input`; `None` if
    /// the step doesn't take it.
    async fn sends(&self, user_id: i64, input: Input) -> (Option<Vec<Message>>, bool) {
        let line = self.line.client();
        let mut answered = None;
        let (_, essential) = self
            .handle(user_id, async |turn| {
                let session = turn.flow.as_ref().and_then(|flow| flow.read::<SlipFlow>());
                let session = session.expect("sending a slip");
                answered = answer(turn, &self.config, &self.ai, &line, session, &input).await?;
                Ok(answered.clone().unwrap_or_default())
            })
            .await;
        (answered, essential)
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(user_id, input).await.0.expect("an answer")
    }

    /// User `user_id` sends a photo as the answer to their step.
    async fn sends_photo(&self, user_id: i64, message_id: &str) -> (Vec<Message>, bool) {
        self.serves(message_id);
        let input = Input::Image {
            message_id: message_id.to_owned(),
        };
        let (answered, essential) = self.sends(user_id, input).await;
        (answered.expect("an answer"), essential)
    }

    /// The buyer sends a photo no flow takes, while `order` awaits their transfer (FR-UI-14).
    async fn sends_photo_unasked(&self, message_id: &str, order: &order::Model) -> Vec<Message> {
        self.serves(message_id);
        let line = self.line.client();
        let (messages, _) = self
            .handle(self.buyer, async |turn| {
                photo_sent(turn, &self.config, &line, message_id, order).await
            })
            .await;
        messages
    }

    /// The buyer taps "ส่งสลิป" on `order` and sends the photo.
    async fn sends_slip(&self, order: &order::Model) -> payment_slip::Model {
        self.taps(self.buyer, &data(SEND_SLIP, order.id)).await;
        self.sends_photo(self.buyer, "slip").await;
        self.slips(order.id).await.pop().expect("a slip")
    }

    /// The step of sending a slip the user is on, if any.
    async fn step(&self, user_id: i64) -> Option<(Step, SlipFlow)> {
        let row = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()?;
        assert_eq!(row.flow, SlipFlow::NAME);
        let step = serde_json::from_value(Json::String(row.step)).unwrap();
        Some((step, serde_json::from_value(row.data).unwrap()))
    }

    async fn get(&self, id: i64) -> order::Model {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Order `id`'s slips, oldest first.
    async fn slips(&self, id: i64) -> Vec<payment_slip::Model> {
        payment_slip::Entity::find()
            .filter(payment_slip::Column::OrderId.eq(id))
            .order_by_asc(payment_slip::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    /// User `user_id`'s drafts.
    async fn drafts(&self, user_id: i64) -> Vec<Uuid> {
        image::Entity::find()
            .filter(image::Column::OwnerType.eq(OwnerType::Draft))
            .filter(image::Column::OwnerId.eq(user_id))
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|image| image.id)
            .collect()
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

    async fn forget_pushes(&self) {
        push_log::Entity::delete_many()
            .exec(&self.db)
            .await
            .unwrap();
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
                line::Action::Postback { label, data, .. } => (label.as_str(), data.as_str()),
                action => panic!("not a postback: {action:?}"),
            },
            other => panic!("not a button: {other:?}"),
        })
        .collect()
}

/// The lines and buttons of the notice pushed as `messages[i]`, as LINE gets it.
fn pushed(messages: &Json, i: usize) -> (Vec<String>, Vec<(String, String)>) {
    let card = &messages[i]["contents"];
    let texts = card["body"]["contents"].as_array().unwrap().iter();
    let lines = texts.map(|text| text["text"].as_str().unwrap().to_owned());
    let footer = card["footer"]["contents"].as_array();
    let buttons = footer.into_iter().flatten().map(|button| {
        let action = &button["action"];
        let field = |name: &str| action[name].as_str().unwrap().to_owned();
        (field("label"), field("data"))
    });
    (lines.collect(), buttons.collect())
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

/// FR-PAY-05, AC-14: the buyer taps "ส่งสลิป" and sends the photo; the seller gets it as an image
/// behind a link that lasts `image.slipLinkHours`, then the notice with "ได้รับเงินแล้ว" and
/// "ยังไม่ได้รับ"; the order awaits their confirmation, and the buyer is told so.
#[tokio::test]
async fn the_buyer_sends_the_slip_and_the_seller_gets_it() {
    let s = setup().await;
    let placed = s.order().await;
    let id = placed.id;

    let asked = s.taps(s.buyer, &data(SEND_SLIP, id)).await;
    assert_eq!(
        text(&asked[0]),
        "ส่งรูปสลิปโอนเงินค่าออเดอร์ IM-20261025-0001 ยอด ฿220 มาในแชตนี้ได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&asked[0]), ["ยกเลิก"]);
    assert_eq!(s.step(s.buyer).await.unwrap().0, Step::Photo);

    // Text is no photo
    let again = s.types(s.buyer, "โอนแล้วค่ะ").await;
    assert_eq!(text(&again[0]), text(&asked[0]));
    assert_eq!(s.get(id).await, placed);

    let (sent, essential) = s.sends_photo(s.buyer, "slip").await;
    assert!(essential);
    assert_eq!(
        lines(&sent[0]),
        [
            "ส่งสลิปออเดอร์ IM-20261025-0001 ให้ร้านแล้วค่ะ",
            "รอร้านตรวจสอบยอดเงินค่ะ สถานะการชำระเงิน: รอร้านตรวจสอบ",
        ]
    );
    // Sent, it can't be sent again until the seller answers
    let (cancel, message) = (data(REQUEST_CANCEL, id), data(MESSAGE_SHOP, id));
    assert_eq!(
        buttons_of(&sent[0]),
        [
            ("ขอยกเลิก", cancel.as_str()),
            ("ส่งข้อความถึงร้าน", message.as_str()),
        ]
    );
    assert_eq!(s.step(s.buyer).await, None);

    let order = s.get(id).await;
    assert_eq!(order.payment_status, PaymentStatus::AwaitingConfirmation);
    let [slip] = <[_; 1]>::try_from(s.slips(id).await).unwrap();
    assert_eq!(slip.status, payment_slip::Status::Submitted);
    assert_eq!(slip.token_expires_at, t0() + TimeDelta::hours(24));
    let image = image::Entity::find_by_id(slip.image_id.unwrap())
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (image.owner_type, image.owner_id),
        (OwnerType::Slip, slip.id)
    );
    assert!(s.drafts(s.buyer).await.is_empty());

    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    let (kind, to, messages) = &pushes[0];
    assert_eq!((kind.as_str(), to.as_str()), ("payment.slip", SELLER));
    let link = images::slip_image_url(&s.config.app.base_url, slip.access_token).to_string();
    assert!(
        link.ends_with(&format!("/img/s/{}", slip.access_token)),
        "{link}"
    );
    assert_eq!(
        messages[0],
        serde_json::json!({
            "type": "image",
            "originalContentUrl": link,
            "previewImageUrl": link,
        })
    );
    let (told, buttons) = pushed(messages, 1);
    assert_eq!(
        told,
        [
            "ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261025-0001 แล้วค่ะ",
            "ยอดออเดอร์ ฿220",
            "ลูกค้า: สมชาย",
            "ตรวจดูว่าเงินเข้าบัญชีแล้ว แล้วกดตอบด้านล่างได้เลยค่ะ รูปสลิปเปิดดูได้ภายใน 24 ชั่วโมง",
        ]
    );
    let not_received = format!("{NOT_PAID}?order={id}&slip={}", slip.id);
    assert_eq!(
        buttons,
        [
            ("ได้รับเงินแล้ว".to_owned(), data(MARK_PAID, id)),
            ("ยังไม่ได้รับ".to_owned(), not_received),
        ]
    );

    // A second tap finds it sent
    let now = s.taps(s.buyer, &data(SEND_SLIP, id)).await;
    assert_eq!(
        lines(&now[0]),
        [
            "ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"ร้านรับออเดอร์แล้ว\"ค่ะ",
            "การชำระเงิน: รอร้านตรวจสอบ",
        ]
    );
    assert_eq!(s.step(s.buyer).await, None);
}

/// FR-UI-14 step 3: a photo the buyer sends unasked while their order awaits a transfer is asked
/// about; "ไม่ใช่" deletes it and changes nothing; "ใช่" sends it as the slip; another photo is
/// asked about instead.
#[tokio::test]
async fn a_photo_sent_unasked_is_asked_about() {
    let s = setup().await;
    let placed = s.order().await;

    let asked = s.sends_photo_unasked("p1", &placed).await;
    assert_eq!(
        text(&asked[0]),
        "ได้รับรูปแล้วค่ะ รูปนี้เป็นสลิปโอนเงินค่าออเดอร์ IM-20261025-0001 ยอด ฿220 ใช่ไหมคะ ถ้าใช่ ระบบจะส่งให้ร้านตรวจสอบค่ะ"
    );
    assert_eq!(quick_replies(&asked[0]), ["ใช่", "ไม่ใช่", "ยกเลิก"]);
    let (step, draft) = s.step(s.buyer).await.unwrap();
    assert_eq!(step, Step::Confirm);
    assert_eq!(
        draft.photo.map(|photo| vec![photo]),
        Some(s.drafts(s.buyer).await)
    );

    let not = s.types(s.buyer, "ไม่ใช่").await;
    assert_eq!(
        text(&not[0]),
        "รับทราบค่ะ ไม่ได้ส่งรูปนี้ให้ร้าน เลือกเมนูที่ต้องการได้เลยค่ะ"
    );
    assert_eq!(not.len(), 2, "with the main menu");
    assert_eq!(s.step(s.buyer).await, None);
    assert!(s.drafts(s.buyer).await.is_empty());
    assert_eq!(s.get(placed.id).await, placed);
    assert_eq!(s.pushes().await, []);

    // Another photo while asked is asked about instead
    s.sends_photo_unasked("p2", &placed).await;
    let first = s.drafts(s.buyer).await;
    let (again, _) = s.sends_photo(s.buyer, "p3").await;
    assert_eq!(text(&again[0]), text(&asked[0]));
    let second = s.drafts(s.buyer).await;
    assert_eq!(second.len(), 1);
    assert_ne!(second, first);
    // Anything else asks again
    let again = s.types(s.buyer, "โอนแล้ว").await;
    assert_eq!(text(&again[0]), text(&asked[0]));

    let sent = s.types(s.buyer, "ใช่").await;
    assert_eq!(
        lines(&sent[0])[0],
        "ส่งสลิปออเดอร์ IM-20261025-0001 ให้ร้านแล้วค่ะ"
    );
    let [slip] = <[_; 1]>::try_from(s.slips(placed.id).await).unwrap();
    assert_eq!(slip.image_id, Some(second[0]));
    assert!(s.drafts(s.buyer).await.is_empty());
    assert_eq!(
        s.get(placed.id).await.payment_status,
        PaymentStatus::AwaitingConfirmation
    );
    assert_eq!(s.pushes().await[0].0, "payment.slip");
}

/// FR-PAY-05: "ยังไม่ได้รับ" puts the order back to awaiting a transfer, and the buyer is told,
/// with "ส่งสลิป" to send another; tapped again it changes nothing, and on the notice of a slip
/// since replaced it refuses nothing the seller hasn't seen.
#[tokio::test]
async fn the_seller_did_not_receive_it_and_the_buyer_is_told() {
    let s = setup().await;
    let placed = s.order().await;
    let id = placed.id;
    let first = s.sends_slip(&placed).await;
    s.forget_pushes().await;

    let not_received = format!("{NOT_PAID}?order={id}&slip={}", first.id);
    let (answered, essential) = s.tapped(s.seller, &not_received).await;
    assert!(essential);
    assert_eq!(
        lines(&answered[0]),
        [
            "บันทึกว่ายังไม่ได้รับเงินออเดอร์ IM-20261025-0001ค่ะ",
            "แจ้งลูกค้าให้ตรวจสอบการโอนแล้วค่ะ",
        ]
    );
    let order = s.get(id).await;
    assert_eq!(order.payment_status, PaymentStatus::Unpaid);
    assert_eq!(s.slips(id).await[0].status, payment_slip::Status::Rejected);
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    let (kind, to, messages) = &pushes[0];
    // Always sent, whatever `notification.orderStatusToBuyer` says
    assert_eq!(
        (kind.as_str(), to.as_str()),
        ("payment.slip_not_received", BUYER)
    );
    let (told, buttons) = pushed(messages, 0);
    assert_eq!(
        told,
        [
            "ร้านป้าแดงแจ้งว่ายังไม่ได้รับเงินค่าออเดอร์ IM-20261025-0001ค่ะ",
            "ยอดที่ต้องโอน ฿220",
            "ตรวจสอบการโอนอีกครั้ง หรือสอบถามร้านได้โดยตรงค่ะ",
        ]
    );
    assert_eq!(buttons[0], ("ส่งสลิป".to_owned(), data(SEND_SLIP, id)));

    // Tapped again
    let again = s.taps(s.seller, &not_received).await;
    assert_eq!(lines(&again[0])[1], "การชำระเงิน: ยังไม่ชำระ");
    assert_eq!(s.get(id).await, order);

    // The buyer sends another; the first one's notice no longer refuses it
    let second = s.sends_slip(&placed).await;
    s.forget_pushes().await;
    let stale = s.taps(s.seller, &not_received).await;
    assert_eq!(lines(&stale[0])[1], "การชำระเงิน: รอร้านตรวจสอบ");
    assert_eq!(s.slips(id).await[1].status, payment_slip::Status::Submitted);
    assert_eq!(s.pushes().await, []);

    // Its own does
    let not_received = format!("{NOT_PAID}?order={id}&slip={}", second.id);
    s.taps(s.seller, &not_received).await;
    assert_eq!(s.slips(id).await[1].status, payment_slip::Status::Rejected);
}

/// FR-PAY-05, FR-PAY-08: "ได้รับเงินแล้ว" records the order paid and confirms the slip; the buyer
/// is told.
#[tokio::test]
async fn the_seller_received_it() {
    let s = setup().await;
    let placed = s.order().await;
    s.sends_slip(&placed).await;
    s.forget_pushes().await;

    let paid = s.taps(s.seller, &data(MARK_PAID, placed.id)).await;
    assert_eq!(
        lines(&paid[0]),
        ["บันทึกว่าได้รับเงินออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    assert_eq!(s.get(placed.id).await.payment_status, PaymentStatus::Paid);
    assert_eq!(
        s.slips(placed.id).await[0].status,
        payment_slip::Status::Confirmed
    );
    let pushes = s.pushes().await;
    assert_eq!(
        (pushes[0].0.as_str(), pushes[0].1.as_str()),
        ("order.status", BUYER)
    );
}

/// The acceptance criterion of #41, FR-PAY-08: only the shop's seller, or an admin, confirms the
/// payment; its buyer and another seller can't, nor say it wasn't received. An admin's is
/// audited.
#[tokio::test]
async fn only_the_seller_or_an_admin_confirms_the_payment() {
    let s = setup().await;
    let placed = s.order().await;
    let slip = s.sends_slip(&placed).await;
    let id = placed.id;
    let awaiting = s.get(id).await;
    let not_received = format!("{NOT_PAID}?order={id}&slip={}", slip.id);

    for user in [s.buyer, s.other_seller, s.other_buyer] {
        for tap in [data(MARK_PAID, id), not_received.clone()] {
            let refused = s.taps(user, &tap).await;
            assert_eq!(
                text(&refused[0]),
                "ขอโทษค่ะ รายการนี้ทำไม่ได้ เลือกจากเมนูด้านล่างได้เลยค่ะ",
                "{user} {tap}"
            );
        }
    }
    assert_eq!(s.get(id).await, awaiting);
    assert_eq!(s.slips(id).await[0].status, payment_slip::Status::Submitted);

    let paid = s.taps(s.admin, &data(MARK_PAID, id)).await;
    assert_eq!(
        lines(&paid[0]),
        ["บันทึกว่าได้รับเงินออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    assert_eq!(s.get(id).await.payment_status, PaymentStatus::Paid);
    assert_eq!(s.slips(id).await[0].status, payment_slip::Status::Confirmed);
    let audited = audit_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(audited.len(), 1);
    assert_eq!(
        (audited[0].actor_id, audited[0].target_id),
        (Some(s.admin), Some(id))
    );
}

/// NFR-SEC-02, NFR-SEC-10: only the order's buyer sends its slip, while not suspended, and only
/// while it awaits a transfer; a flow in progress ends if either stops being so.
#[tokio::test]
async fn only_its_buyer_sends_a_slip_while_it_awaits_a_transfer() {
    let s = setup().await;
    let placed = s.order().await;
    let id = placed.id;

    let refused = s.taps(s.other_buyer, &data(SEND_SLIP, id)).await;
    assert_eq!(
        text(&refused[0]),
        "ขอโทษค่ะ รายการนี้ทำไม่ได้ เลือกจากเมนูด้านล่างได้เลยค่ะ"
    );
    assert_eq!(s.step(s.other_buyer).await, None);

    // Cash isn't sent a slip of
    let cash = s.order_paid_by(Status::Accepted, PaymentMethod::Cash).await;
    let now = s.taps(s.buyer, &data(SEND_SLIP, cash.id)).await;
    assert_eq!(lines(&now[0])[1], "การชำระเงิน: ยังไม่ชำระ");
    assert_eq!(s.step(s.buyer).await, None);

    // Cancelled meanwhile, the flow ends with the photo unsent
    s.sends_photo_unasked("p1", &placed).await;
    let mut cancelled: order::ActiveModel = placed.clone().into();
    cancelled.status = Set(Status::Cancelled);
    cancelled.update(&s.db).await.unwrap();
    let ended = s.types(s.buyer, "ใช่").await;
    assert_eq!(
        lines(&ended[0])[0],
        "ออเดอร์ IM-20261025-0001 ตอนนี้สถานะ \"ยกเลิกแล้ว\"ค่ะ"
    );
    assert_eq!(s.step(s.buyer).await, None);
    assert!(s.drafts(s.buyer).await.is_empty());
    assert!(s.slips(id).await.is_empty());

    // Suspended mid-flow
    let other = s
        .order_paid_by(Status::Delivering, PaymentMethod::PromptPay)
        .await;
    s.taps(s.buyer, &data(SEND_SLIP, other.id)).await;
    let suspended = user::ActiveModel {
        id: sea_orm::ActiveValue::Unchanged(s.buyer),
        status: Set(user::Status::Suspended),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();
    let (denied, _) = s.sends_photo(s.buyer, "p2").await;
    assert!(text(&denied[0]).contains("บัญชีของคุณถูกระงับ"));
    assert_eq!(s.step(s.buyer).await, None);
    assert!(s.slips(other.id).await.is_empty());
    let photo = s.sends_photo_unasked("p3", &other).await;
    assert!(text(&photo[0]).contains("บัญชีของคุณถูกระงับ"));
    assert!(s.drafts(s.buyer).await.is_empty());
}

/// The lines of the notice pushed as `messages[i]`, each with its colour and whether it is bold,
/// as LINE gets it.
fn toned(messages: &Json, i: usize) -> Vec<(String, Option<String>, bool)> {
    let texts = messages[i]["contents"]["body"]["contents"]
        .as_array()
        .unwrap();
    texts
        .iter()
        .map(|text| {
            let line = text["text"].as_str().unwrap().to_owned();
            let color = text["color"].as_str().map(str::to_owned);
            (line, color, text["weight"] == "bold")
        })
        .collect()
}

/// The lines of the notice of a slip the AI didn't read, sent by สมชาย for order
/// IM-20261025-0001
const UNREAD: [&str; 4] = [
    "ลูกค้าส่งสลิปโอนเงินออเดอร์ IM-20261025-0001 แล้วค่ะ",
    "ยอดออเดอร์ ฿220",
    "ลูกค้า: สมชาย",
    "ตรวจดูว่าเงินเข้าบัญชีแล้ว แล้วกดตอบด้านล่างได้เลยค่ะ รูปสลิปเปิดดูได้ภายใน 24 ชั่วโมง",
];

/// What the AI read from a slip of ฿200 transferred on 25 ต.ค. 2569 at 08:41
fn read_200() -> Value {
    json!({
        "amount": 200,
        "date": "2026-10-25",
        "time": "08:41",
        "reference": "016298084112ABC",
    })
}

/// The acceptance criterion of #46, FR-PAY-06, FR-AI-06: a slip of the wrong amount stands out
/// to the seller, beside the order's total, and changes nothing: the order awaits their answer,
/// and is paid only once they tap "ได้รับเงินแล้ว". The AI reads the photo kept as the slip, as
/// data, and isn't told what the order expects; the call is the system's.
#[tokio::test]
async fn a_slip_of_the_wrong_amount_stands_out_and_the_seller_decides() {
    let mut s = setup().await;
    let model = s.ai_reads([Scripted::Json(read_200())], &[]);
    let placed = s.order().await;
    let id = placed.id;

    let slip = s.sends_slip(&placed).await;

    // Sent all the same, with what was read kept
    assert_eq!(slip.status, payment_slip::Status::Submitted);
    assert_eq!(
        s.get(id).await.payment_status,
        PaymentStatus::AwaitingConfirmation
    );
    assert_eq!(slip.ai_amount, Some(Money::from_baht(200)));
    let transferred = Utc.with_ymd_and_hms(2026, 10, 25, 1, 41, 0).unwrap();
    assert_eq!(slip.ai_paid_at, Some(transferred));
    assert_eq!(slip.ai_reference.as_deref(), Some("016298084112ABC"));

    let pushes = s.pushes().await;
    let [(kind, to, messages)] = &pushes[..] else {
        panic!("not one push: {pushes:?}");
    };
    assert_eq!((kind.as_str(), to.as_str()), ("payment.slip", SELLER));
    let warning = Some("#D93025".to_owned());
    let muted = Some("#888888".to_owned());
    let plain = |line: &str| (line.to_owned(), None, false);
    assert_eq!(
        toned(messages, 1),
        [
            (UNREAD[0].to_owned(), None, true),
            plain(UNREAD[1]),
            (
                "ยอดในสลิป ฿200 ⚠️ ไม่ตรงกับยอดออเดอร์".to_owned(),
                warning,
                true
            ),
            plain("โอนเมื่อ 25 ต.ค. 2569 08:41"),
            ("เลขอ้างอิง 016298084112ABC".to_owned(), muted.clone(), false),
            (
                "ระบบอ่านจากสลิปอัตโนมัติ อาจผิดพลาดได้ ตรวจยอดในบัญชีก่อนกดตอบนะคะ".to_owned(),
                muted,
                false
            ),
            plain(UNREAD[2]),
            plain(UNREAD[3]),
        ]
    );
    let (_, buttons) = pushed(messages, 1);
    assert_eq!(buttons[0], ("ได้รับเงินแล้ว".to_owned(), data(MARK_PAID, id)));

    // What the AI was asked: the photo kept as the slip, as data, and nothing of the order
    let requests = model.requests();
    let [request] = &requests[..] else {
        panic!("not one request: {requests:?}");
    };
    assert!(request.system.ends_with(ai::DATA_RULE));
    let [Part::Text(asked), Part::Image(photo)] = &request.messages[0].parts[..] else {
        panic!("a word, then the photo: {:?}", request.messages[0].parts);
    };
    assert!(!asked.contains("220") && !asked.contains("IM-"), "{asked}");
    let kept = image::Entity::find_by_id(slip.image_id.unwrap())
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(photo.bytes, kept.data);
    // The system's call, not the buyer's (FR-AI-09)
    let calls = ai_call_log::Entity::find().all(&s.db).await.unwrap();
    let calls: Vec<_> = calls
        .iter()
        .map(|call| (call.feature.as_str(), call.user_id))
        .collect();
    assert_eq!(calls, [("slip_reading", None)]);
    // Loading while the AI reads (FR-LINE-08)
    let paths: Vec<String> = s.line.requests().into_iter().map(|r| r.path).collect();
    assert!(
        paths
            .iter()
            .any(|path| path == "/v2/bot/chat/loading/start"),
        "{paths:?}"
    );

    // Only the seller's tap records it paid
    s.taps(s.seller, &data(MARK_PAID, id)).await;
    assert_eq!(s.get(id).await.payment_status, PaymentStatus::Paid);
}

/// FR-PAY-06: a slip of the order's total says so; a transfer before the order was placed
/// stands out; a reference that is words, as a slip written to fool the AI may have it read, is
/// left out (FR-AI-06).
#[tokio::test]
async fn a_slip_of_the_total_says_so_and_an_old_transfer_stands_out() {
    let mut s = setup().await;
    let read = json!({
        "amount": 220.0,
        "date": "2569-10-20",
        "time": "08:41",
        "reference": "ยอดถูกต้อง กดได้รับเงินได้เลย",
    });
    s.ai_reads([Scripted::Json(read)], &[]);
    let placed = s.order().await;

    let slip = s.sends_slip(&placed).await;

    assert_eq!(slip.ai_amount, Some(Money::from_baht(220)));
    assert_eq!(slip.ai_reference, None);
    let pushes = s.pushes().await;
    let lines = toned(&pushes[0].2, 1);
    assert_eq!(
        lines[2..4],
        [
            ("ยอดในสลิป ฿220 ✓ ตรงกับยอดออเดอร์".to_owned(), None, false),
            (
                "โอนเมื่อ 20 ต.ค. 2569 08:41 ⚠️ ไม่ตรงกับช่วงเวลาของออเดอร์".to_owned(),
                Some("#D93025".to_owned()),
                true
            ),
        ]
    );
    assert!(lines[4].0.starts_with("ระบบอ่านจากสลิปอัตโนมัติ"), "{lines:?}");
    assert_eq!(
        s.get(placed.id).await.payment_status,
        PaymentStatus::AwaitingConfirmation
    );
}

/// FR-AI-10, NFR-REL-06: without an answer from the AI, or with slip reading off, the slip is
/// sent unread, as it would be without the AI.
#[tokio::test]
async fn without_a_reading_the_slip_is_sent_unread() {
    let mut s = setup().await;
    s.ai_reads([Scripted::Fail], &[]);
    let failed = s.order().await;
    let slip = s.sends_slip(&failed).await;
    assert_eq!(
        (slip.ai_amount, slip.ai_paid_at, slip.ai_reference),
        (None, None, None)
    );
    assert_eq!(
        s.get(failed.id).await.payment_status,
        PaymentStatus::AwaitingConfirmation
    );
    let pushes = s.pushes().await;
    assert_eq!(pushed(&pushes[0].2, 1).0, UNREAD);
    s.forget_pushes().await;

    let model = s.ai_reads([Scripted::Json(read_200())], &[Feature::SlipReading]);
    let off = s.order().await;
    let slip = s.sends_slip(&off).await;
    assert!(model.requests().is_empty());
    assert_eq!(slip.ai_amount, None);
    let pushes = s.pushes().await;
    let unread = UNREAD.map(|line| line.replace("0001", "0002"));
    assert_eq!(pushed(&pushes[0].2, 1).0, unread);
}
