//! "ข้อมูลของฉัน" (FR-ONB-05): every personal-data field the entity models mark (DR-10) is
//! shown, and its buttons correct the address, phone and location. Each test starts a throwaway
//! PostgreSQL, so they need Docker.

use std::collections::BTreeSet;

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ActiveModelTrait, DatabaseConnection, IdenStatic, TransactionTrait};
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::flow::Flow;
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Action, Body, limits};
use crate::notifications::templates::Templates;
use crate::persistence::entity::{PersonalData, image, push_log, shop, user, webhook_event};
use crate::persistence::{orm, random_id, testing};
use crate::users::access::Actor;
use crate::users::address::AddressFlow;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ALICE: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BOB: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    config: Config,
}

/// The sample config, with the AI on.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let config = config::sample();
    Setup {
        _container: container,
        db: orm(pool),
        templates: Templates::new(&config.app),
        config,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh.
    async fn at<T>(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>, &Config) -> Result<T, DbErr>,
    ) -> T {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let result = handle(&turn, &self.config).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        result
    }

    /// "ข้อมูลของฉัน" of user `user_id`.
    async fn my_data(&self, user_id: i64) -> Message {
        let mut reply = self
            .at(user_id, async |turn, config| {
                show(turn, config, &turn.actor.user).await
            })
            .await;
        assert_eq!(reply.len(), 1);
        reply.remove(0)
    }

    async fn taps(&self, user_id: i64, postback: &Postback) -> Vec<Message> {
        self.at(user_id, async |turn, config| {
            tapped(turn, config, postback).await
        })
        .await
    }
}

/// The lines of the card: each section's title, its rows as `label: value`, and its other
/// lines.
fn card_lines(message: &Message) -> Vec<String> {
    let Body::Flex {
        contents: FlexContainer::Bubble(card),
        ..
    } = &message.body
    else {
        panic!("not a card: {message:?}");
    };
    let mut lines = Vec::new();
    for component in &card.body.as_ref().expect("a body").contents {
        let section = match component {
            Component::Box(section) => section,
            Component::Separator(_) => continue,
            other => panic!("not a section: {other:?}"),
        };
        for part in &section.contents {
            match part {
                Component::Text(text) => lines.push(text.text.clone()),
                Component::Box(row) => match row.contents.as_slice() {
                    [Component::Text(label), Component::Text(value)] => {
                        lines.push(format!("{}: {}", label.text, value.text));
                    }
                    other => panic!("not a row: {other:?}"),
                },
                other => panic!("not a row or line: {other:?}"),
            }
        }
    }
    lines
}

/// The postback data of the card's buttons.
fn buttons(message: &Message) -> Vec<String> {
    let Body::Flex {
        contents: FlexContainer::Bubble(card),
        ..
    } = &message.body
    else {
        panic!("not a card: {message:?}");
    };
    let footer = card.footer.as_ref().expect("buttons");
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { data, .. } => data.clone(),
                action => panic!("not a postback: {action:?}"),
            },
            other => panic!("not a button: {other:?}"),
        })
        .collect()
}

/// The personal-data fields of `E`, as `(table, column)` (DR-10).
fn fields<E: PersonalData + Default>() -> Vec<(String, &'static str)> {
    let table = E::default().table_name().to_owned();
    E::FIELDS
        .iter()
        .map(|column| (table.clone(), column.as_str()))
        .collect()
}

/// Alice, with every personal-data field of her own and her shop's, an order delivered to her
/// with its payment slip, a notification pushed to her and a message she sent.
async fn everything(db: &DatabaseConnection) -> i64 {
    let mut alice = testing::new_user(ALICE);
    alice.display_name = Set(Some("ป้าแดง".to_owned()));
    alice.picture_url = Set(Some("https://profile.line-scdn.net/0halice".to_owned()));
    alice.consent_version = Set(Some("1".to_owned()));
    alice.consented_at = Set(Some(testing::at()));
    alice.zone = Set(Some("ซอย 3".to_owned()));
    alice.house_number = Set(Some("45/2".to_owned()));
    alice.landmark_note = Set(Some("รั้วสีเขียว".to_owned()));
    alice.phone = Set(Some("0812345678".to_owned()));
    alice.latitude = Set(Some(13.75));
    alice.longitude = Set(Some(100.5));
    let alice = alice.insert(db).await.unwrap().id;

    let mut her_shop = testing::new_shop(alice);
    her_shop.phone = Set(Some("053123456".to_owned()));
    her_shop.promptpay_id = Set(Some("0899999999".to_owned()));
    her_shop.pickup_address = Set(Some("หน้าบ้านเลขที่ 45/2".to_owned()));
    her_shop.pickup_latitude = Set(Some(13.7));
    her_shop.pickup_longitude = Set(Some(100.6));
    her_shop.insert(db).await.unwrap();

    let bob = testing::new_user(BOB).insert(db).await.unwrap().id;
    let bobs_shop = testing::new_shop(bob).insert(db).await.unwrap().id;
    let mut order = testing::new_order(alice, bobs_shop, "IM-20261022-0001", random_id());
    order.fulfillment = Set(order::Fulfillment::Delivery);
    order.delivery_zone = Set(Some("ซอย 3".to_owned()));
    order.delivery_house_number = Set(Some("45/2".to_owned()));
    order.delivery_landmark_note = Set(Some("รั้วสีเขียว".to_owned()));
    order.delivery_latitude = Set(Some(13.75));
    order.delivery_longitude = Set(Some(100.5));
    let order = order.insert(db).await.unwrap().id;
    let slip = payment_slip::ActiveModel {
        order_id: Set(order),
        image_id: Set(None),
        access_token: Set(random_id()),
        token_expires_at: Set(testing::at()),
        ai_reference: Set(Some("REF123".to_owned())),
        status: Set(payment_slip::Status::Submitted),
        created_at: Set(testing::at()),
        ..Default::default()
    };
    let slip = slip.insert(db).await.unwrap();
    let image = image::ActiveModel {
        id: Set(random_id()),
        owner_type: Set(image::OwnerType::Slip),
        owner_id: Set(slip.id),
        position: Set(0),
        content_type: Set("image/jpeg".to_owned()),
        width: Set(1),
        height: Set(1),
        size_bytes: Set(1),
        data: Set(vec![0xff]),
        created_at: Set(testing::at()),
    };
    let image = image.insert(db).await.unwrap();
    let mut slip: payment_slip::ActiveModel = slip.into();
    slip.image_id = Set(Some(image.id));
    slip.update(db).await.unwrap();

    let push = push_log::ActiveModel {
        push_type: Set("ORDER_STATUS".to_owned()),
        recipient: Set(ALICE.to_owned()),
        status: Set(push_log::Status::Pending),
        sent_at: Set(testing::at()),
        messages: Set(Some(
            json!([{ "type": "text", "text": "ร้านรับออเดอร์แล้วค่ะ" }]),
        )),
        attempts: Set(0),
        queued_at: Set(testing::at()),
        ..Default::default()
    };
    push.insert(db).await.unwrap();
    let said = json!({ "webhookEventId": "e1", "type": "message", "message": { "text": "ขอไข่" } });
    testing::new_event(ALICE, said, testing::at())
        .insert(db)
        .await
        .unwrap();
    alice
}

/// AC (FR-ONB-05, DR-10): "ข้อมูลของฉัน" lists every personal-data field marked in the entity
/// models: the user's own and their shop's as they are, and those of other records by what
/// holds them. A field marked later fails this test until the card shows it.
#[tokio::test]
async fn my_data_lists_every_personal_data_field() {
    let s = setup().await;
    let alice = everything(&s.db).await;
    let card = s.my_data(alice).await;
    limits::check(std::slice::from_ref(&card)).unwrap();
    let lines = card_lines(&card);

    let location = "ตำแหน่ง: 13.75000, 100.50000";
    let pickup = "ตำแหน่งรับของ: 13.70000, 100.60000";
    let orders = "ที่อยู่ส่งของใน 1 คำสั่งซื้อ";
    let slips = "รูปสลิปโอนเงิน 1 รูป เก็บ 90 วัน";
    let pushes = "การแจ้งเตือนที่ส่งถึงคุณ เก็บ 90 วัน";
    let events = "ข้อความที่คุณส่งถึงระบบ เก็บ 14 วัน";
    let line_user_id = format!("LINE user ID: {ALICE}");
    let shown = [
        (("users", "line_user_id"), line_user_id.as_str()),
        (("users", "display_name"), "ชื่อ: ป้าแดง"),
        (("users", "picture_url"), "รูปโปรไฟล์: มี"),
        (("users", "zone"), "โซน: ซอย 3"),
        (("users", "house_number"), "บ้านเลขที่: 45/2"),
        (("users", "landmark_note"), "จุดสังเกต: รั้วสีเขียว"),
        (("users", "phone"), "เบอร์โทร: 081-234-5678"),
        (("users", "latitude"), location),
        (("users", "longitude"), location),
        (("shops", "promptpay_id"), "พร้อมเพย์: 0899999999"),
        (("shops", "phone"), "เบอร์ร้าน: 053-123-456"),
        (("shops", "pickup_address"), "ที่อยู่รับของ: หน้าบ้านเลขที่ 45/2"),
        (("shops", "pickup_latitude"), pickup),
        (("shops", "pickup_longitude"), pickup),
        (("orders", "delivery_zone"), orders),
        (("orders", "delivery_house_number"), orders),
        (("orders", "delivery_landmark_note"), orders),
        (("orders", "delivery_latitude"), orders),
        (("orders", "delivery_longitude"), orders),
        (("images", "data"), slips),
        (("payment_slips", "image_id"), slips),
        (("payment_slips", "ai_reference"), slips),
        (("push_logs", "recipient"), pushes),
        (("push_logs", "messages"), pushes),
        (("webhook_events", "user_key"), events),
        (("webhook_events", "payload"), events),
    ];

    let marked: BTreeSet<_> = [
        fields::<user::Entity>(),
        fields::<shop::Entity>(),
        fields::<order::Entity>(),
        fields::<image::Entity>(),
        fields::<payment_slip::Entity>(),
        fields::<push_log::Entity>(),
        fields::<webhook_event::Entity>(),
    ]
    .concat()
    .into_iter()
    .collect();
    let covered: BTreeSet<_> = shown
        .iter()
        .map(|&((table, column), _)| (table.to_owned(), column))
        .collect();
    assert_eq!(covered, marked, "every marked field, and only those");
    for ((table, column), line) in shown {
        assert!(
            lines.iter().any(|shown| shown.starts_with(line)),
            "{table}.{column}: no {line:?} in {lines:#?}"
        );
    }
    // Consent, which isn't personal data but is the user's to see (FR-ONB-02)
    assert!(lines.contains(&"ความยินยอม: ฉบับที่ 1 เมื่อ 22 ต.ค. 2569 14:30".to_owned()));
    assert!(lines.contains(&"ข้อความที่คุยกับผู้ช่วย AI เก็บ 30 วัน".to_owned()));
}

/// What the user hasn't given is said to be missing, and what holds none of their data isn't
/// listed.
#[tokio::test]
async fn my_data_says_what_is_not_given() {
    let mut s = setup().await;
    let alice = testing::new_user(ALICE).insert(&s.db).await.unwrap().id;
    let bob = testing::new_user(BOB).insert(&s.db).await.unwrap().id;
    // A pickup order keeps no address, and a slip whose image retention deleted holds nothing
    let bobs_shop = testing::new_shop(bob).insert(&s.db).await.unwrap().id;
    let order = testing::new_order(alice, bobs_shop, "IM-20261022-0001", random_id());
    let order = order.insert(&s.db).await.unwrap().id;
    let slip = payment_slip::ActiveModel {
        order_id: Set(order),
        access_token: Set(random_id()),
        token_expires_at: Set(testing::at()),
        status: Set(payment_slip::Status::Confirmed),
        created_at: Set(testing::at()),
        ..Default::default()
    };
    slip.insert(&s.db).await.unwrap();

    let card = s.my_data(alice).await;
    let lines = card_lines(&card);
    for line in [
        "ชื่อ: ไม่มี",
        "รูปโปรไฟล์: ไม่มี",
        "ความยินยอม: ไม่มี",
        "โซน: ยังไม่ได้ให้ไว้",
        "บ้านเลขที่: ยังไม่ได้ให้ไว้",
        "จุดสังเกต: ยังไม่ได้ให้ไว้",
        "เบอร์โทร: ยังไม่ได้ให้ไว้",
        "ตำแหน่ง: ยังไม่ได้ให้ไว้",
    ] {
        assert!(
            lines.contains(&line.to_owned()),
            "no {line:?} in {lines:#?}"
        );
    }
    for absent in ["ร้านของฉัน", "เบอร์ร้าน", "คำสั่งซื้อ", "สลิป"]
    {
        assert!(
            !lines.iter().any(|line| line.contains(absent)),
            "{absent:?} in {lines:#?}"
        );
    }
    assert_eq!(
        buttons(&card),
        [
            "my_data.edit?part=address",
            "my_data.edit?part=phone",
            "my_data.edit?part=location",
        ]
    );

    // With the AI off, nothing is sent to it, so it keeps nothing
    s.config.ai.primary = None;
    let lines = card_lines(&s.my_data(alice).await);
    assert!(!lines.iter().any(|line| line.contains("AI")), "{lines:#?}");
}

/// FR-ONB-05, NFR-PRIV-03: the card and its buttons, for suspended users too.
#[tokio::test]
async fn its_buttons_correct_the_address_phone_and_location() {
    let s = setup().await;
    let mut alice = testing::new_user(ALICE);
    alice.status = Set(user::Status::Suspended);
    let alice = alice.insert(&s.db).await.unwrap().id;

    let card = s.my_data(alice).await;
    assert_eq!(s.taps(alice, &Postback::new(SHOW)).await, [card]);

    for (part, ask) in [
        ("address", Ask::Address),
        ("phone", Ask::Phone),
        ("location", Ask::Location),
    ] {
        let edit = Postback::new(EDIT).with_name(PART, part);
        let reply = s.taps(alice, &edit).await;
        let expected = s
            .at(alice, async |turn, config| {
                address::start(turn, config, ask, Then::MyData).await
            })
            .await;
        assert_eq!(reply, expected, "{part}");
        let flow = s
            .at(alice, async |turn, _| {
                Ok(turn.flow.as_ref().map(|flow| flow.flow().to_owned()))
            })
            .await;
        assert_eq!(flow.as_deref(), Some(AddressFlow::NAME));
    }

    let gone = [s.templates.denied(Denied::NotFound)];
    for postback in [
        Postback::new(EDIT),
        Postback::new(EDIT).with_name(PART, "zone"),
        Postback::new("my_data.other"),
    ] {
        assert_eq!(s.taps(alice, &postback).await, gone, "{postback}");
    }
}
