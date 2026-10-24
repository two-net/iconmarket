//! "เปิด/ปิดร้าน" and "ตั้งค่าร้าน" (FR-SHOP-05 – FR-SHOP-09): opening and closing at a tap, which
//! leaves orders and products alone; each part of the shop saved once answered; the phone shown
//! or hidden; the card buyers see; and only the owner of an approved shop doing any of it. Each
//! event is handled in a transaction committed once it is handled. The `async` tests each start
//! a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::{ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::catalog::products::Availability;
use crate::catalog::shops;
use crate::config::{self, PaymentMethod, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer, Hero};
use crate::line::testing::{self as fake, Fake};
use crate::line::{Body, limits};
use crate::notifications::templates::Templates;
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{order, product, session, user};
use crate::persistence::{orm, testing};
use crate::thai::Money;
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../../ai/provider/fixtures/media/slip.jpg");

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

#[test]
fn every_part_has_its_own_postback_and_label() {
    for step in PARTS {
        assert_eq!(part(step.key()), Some(step));
        let data = Postback::new(PART).with_name(WHICH, step.key()).to_string();
        assert!(data.len() <= limits::ACTION_TEXT_CHARS, "{data}");
    }
    assert_eq!(part("summary"), None);
    let labels: Vec<_> = PARTS.iter().map(|part| part.label()).collect();
    for (i, label) in labels.iter().enumerate() {
        assert!(!labels[..i].contains(label), "{label:?}");
    }
    assert_eq!(switch(true).label(), &labels::CLOSE_SHOP);
    assert_eq!(switch(false).label(), &labels::REOPEN_SHOP);
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    templates: Templates,
    /// The owner of [`Setup::shop`], with an address and a phone
    seller: i64,
    /// Approved and open, picked up or delivered for ฿10, as Appendix A.5's
    shop: i64,
    /// A user without a shop
    buyer: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = user::ActiveModel {
        display_name: Set(Some("ป้าแดง".to_owned())),
        zone: Set(Some("ซอย 3".to_owned())),
        house_number: Set(Some("45/2".to_owned())),
        phone: Set(Some("0812345678".to_owned())),
        ..testing::new_user(SELLER)
    };
    let seller = seller.insert(&db).await.unwrap().id;
    let shop = shop::ActiveModel {
        phone: Set(Some("0812345678".to_owned())),
        pickup_address: Set(Some("ซอย 3 บ้านเลขที่ 45/2".to_owned())),
        promptpay_id: Set(Some("0812345678".to_owned())),
        ..testing::new_shop(seller)
    };
    let shop = shop.insert(&db).await.unwrap().id;
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    let config = config::sample();
    Setup {
        _container: container,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        seller,
        shop,
        buyer,
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

    /// User `user_id` taps a button sending `data` at `now`.
    async fn taps_at(&self, user_id: i64, data: &str, now: DateTimeUtc) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle_at(user_id, now, async |turn| {
            tapped(turn, &self.config, &postback).await
        })
        .await
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.taps_at(user_id, data, t0()).await
    }

    /// User `user_id` answers the part they are changing with `input`; `None` if the step
    /// doesn't take it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let line = self.line.client();
        let mut answered = None;
        self.handle_at(user_id, t0(), async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<SettingsFlow>());
            let session = session.expect("changing a part of the shop");
            answered = answer(turn, &self.config, &line, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(user_id, input).await.expect("an answer")
    }

    /// User `user_id` sends a photo, which LINE serves as `bytes`.
    async fn sends_photo(&self, user_id: i64, message_id: &str, bytes: &[u8]) -> Vec<Message> {
        let path = format!("/v2/bot/message/{message_id}/content");
        self.line
            .script(&path, [fake::Answer::bytes("image/jpeg", bytes)]);
        let input = Input::Image {
            message_id: message_id.to_owned(),
        };
        self.sends(user_id, input).await.expect("an answer")
    }

    async fn shop(&self) -> shop::Model {
        shop::Entity::find_by_id(self.shop)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Sets the shop's `status`, as an admin's decision would.
    async fn set_status(&self, status: shop::Status) {
        let mut shop = self.shop().await.into_active_model();
        shop.status = Set(status);
        shop.update(&self.db).await.unwrap();
    }

    async fn flow(&self, user_id: i64) -> Option<String> {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .map(|row| row.flow)
    }

    /// Every image, as (owner type, owner, ID), oldest first.
    async fn images(&self) -> Vec<(OwnerType, i64, Uuid)> {
        image::Entity::find()
            .order_by_asc(image::Column::CreatedAt)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.owner_type, row.owner_id, row.id))
            .collect()
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

/// The postback data of `message`'s Quick Reply buttons; `None` for those that aren't.
fn quick_reply_data(message: &Message) -> Vec<Option<&str>> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| match &item.action {
            Action::Postback { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect()
}

/// The card of `message`.
fn bubble(message: &Message) -> &crate::line::flex::Bubble {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => bubble,
        body => panic!("not a card: {body:?}"),
    }
}

/// The postback data of the buttons of `message`'s card.
fn buttons(message: &Message) -> Vec<String> {
    let Some(footer) = &bubble(message).footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { data, .. } => data.clone(),
                action => panic!("not a postback: {action:?}"),
            },
            component => panic!("not a button: {component:?}"),
        })
        .collect()
}

/// The link of the photo across the top of `message`'s card.
fn hero(message: &Message) -> Option<&str> {
    match &bubble(message).hero {
        Some(Hero::Image(image)) => Some(&image.url),
        _ => None,
    }
}

/// The texts of `message`'s card, in order, the rows' labels and values among them.
fn card_texts(message: &Message) -> Vec<String> {
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
    for component in &bubble(message).body.as_ref().unwrap().contents {
        texts(component, &mut out);
    }
    out
}

/// The value of the row labelled `label` in `message`'s card.
fn row(message: &Message, label: &str) -> String {
    let texts = card_texts(message);
    let at = texts.iter().position(|text| text == label).expect(label);
    texts[at + 1].clone()
}

/// FR-SHOP-05: "เปิด/ปิดร้าน" closes the shop at a tap, and again opens it; the reply's button
/// switches it back.
#[tokio::test]
async fn the_menu_tap_closes_and_opens_the_shop() {
    let s = setup().await;
    let closed = s.taps(s.seller, "open_close").await;
    assert_eq!(closed.len(), 1);
    assert!(
        text(&closed[0]).starts_with("ปิดร้านป้าแดงแล้วค่ะ ผู้ซื้อยังเห็นสินค้า แต่จะขึ้นว่า \"ร้านปิด\""),
        "{}",
        text(&closed[0])
    );
    assert!(text(&closed[0]).contains("ออเดอร์ที่มีอยู่ยังทำต่อตามปกติ"));
    assert_eq!(quick_replies(&closed[0]), ["เปิดร้าน", "เมนู"]);
    assert_eq!(quick_reply_data(&closed[0])[0], Some("shop.open?to=1"));
    assert!(!s.shop().await.is_open);

    let opened = s
        .taps_at(s.seller, "open_close", t0() + TimeDelta::hours(1))
        .await;
    assert!(text(&opened[0]).starts_with("เปิดร้านป้าแดงแล้วค่ะ ผู้ซื้อสั่งสินค้าได้ตามปกติ"));
    assert_eq!(quick_replies(&opened[0]), ["ปิดร้าน", "เมนู"]);
    assert_eq!(quick_reply_data(&opened[0])[0], Some("shop.open?to=0"));
    let shop = s.shop().await;
    assert!(shop.is_open);
    assert_eq!(shop.updated_at, t0() + TimeDelta::hours(1));

    // "ร้านของฉัน" offers the same switch
    let mine = s.handle_at(s.seller, t0(), async |turn| {
        shops::tapped(turn, &s.config, &Item::MyShop.postback()).await
    });
    let mine = mine.await;
    assert_eq!(quick_reply_data(&mine[1])[0], Some("shop.open?to=0"));
}

/// FR-UI-13: the switch names the state it sets, so a second tap of it changes nothing.
#[tokio::test]
async fn a_repeated_switch_changes_nothing() {
    let s = setup().await;
    s.taps(s.seller, "shop.open?to=0").await;
    let later = t0() + TimeDelta::hours(1);
    let again = s.taps_at(s.seller, "shop.open?to=0", later).await;
    assert!(
        text(&again[0]).starts_with("ร้านป้าแดงปิดอยู่แล้วค่ะ"),
        "{}",
        text(&again[0])
    );
    assert_eq!(quick_replies(&again[0]), ["เปิดร้าน", "เมนู"]);
    let shop = s.shop().await;
    assert!(!shop.is_open);
    assert_eq!(shop.updated_at, t0());

    s.taps_at(s.seller, "shop.open?to=1", later).await;
    let again = s
        .taps_at(s.seller, "shop.open?to=1", later + TimeDelta::hours(1))
        .await;
    assert!(text(&again[0]).starts_with("ร้านป้าแดงเปิดอยู่แล้วค่ะ"));
    assert_eq!(s.shop().await.updated_at, later);
    // Only 0 and 1
    let odd = s.taps(s.seller, "shop.open?to=2").await;
    assert_eq!(odd, [s.templates.denied(Denied::NotFound)]);
}

/// Acceptance (FR-SHOP-05): closing the shop leaves its open orders as they were, and its
/// owner still handles them; its products stay as they are, but can't be ordered until it
/// opens again.
#[tokio::test]
async fn closing_leaves_open_orders_unchanged() {
    let s = setup().await;
    let category = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let eggs = testing::new_product(s.shop, category.id, "ไข่ไก่เบอร์ 2", Some(5));
    let eggs = eggs.insert(&s.db).await.unwrap();
    let new = testing::new_order(s.buyer, s.shop, "IM-0001", Uuid::from_u128(1));
    let new = new.insert(&s.db).await.unwrap();
    let accepted = order::ActiveModel {
        status: Set(order::Status::Accepted),
        ..testing::new_order(s.buyer, s.shop, "IM-0002", Uuid::from_u128(2))
    };
    let accepted = accepted.insert(&s.db).await.unwrap();

    s.taps(s.seller, "open_close").await;
    let orders = order::Entity::find()
        .order_by_asc(order::Column::Id)
        .all(&s.db)
        .await
        .unwrap();
    assert_eq!(orders, [new.clone(), accepted.clone()]);
    let products = product::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(products, std::slice::from_ref(&eggs));

    let actor = Actor::load(&s.db, s.seller).await.unwrap().unwrap();
    assert!(actor.roles().seller);
    for order in [&new, &accepted] {
        let handle = access::Action::HandleOrder(order.id);
        assert_eq!(authorize(&s.db, &actor, handle).await.unwrap(), Ok(()));
    }
    let shop = s.shop().await;
    assert!(!shops::takes_orders(&shop));
    assert_eq!(Availability::of(&eggs, &shop), Availability::ShopClosed);
    assert!(!Availability::of(&eggs, &shop).orderable());

    s.taps(s.seller, "open_close").await;
    assert_eq!(
        Availability::of(&eggs, &s.shop().await),
        Availability::Left(5)
    );
}

/// FR-SHOP-10, NFR-SEC-02: only the owner of an approved shop opens, closes or sets it up; a
/// suspended shop or owner is told why.
#[tokio::test]
async fn only_the_owner_of_an_approved_shop_runs_it() {
    let s = setup().await;
    let taps = [
        "open_close",
        "shop_settings",
        "shop.open?to=0",
        "shop.part?p=name",
        "shop.phone?show=0",
        "shop.front",
    ];
    let refused = async |user_id: i64, denied: Denied| {
        let before = s.shop().await;
        for data in taps {
            let reply = s.taps(user_id, data).await;
            assert_eq!(reply, [s.templates.denied(denied)], "{data}");
            assert_eq!(s.flow(user_id).await, None, "{data}");
        }
        assert_eq!(s.shop().await, before);
    };
    refused(s.buyer, Denied::NotAllowed).await;
    for (status, denied) in [
        (shop::Status::Pending, Denied::NotAllowed),
        (shop::Status::Rejected, Denied::NotAllowed),
        (shop::Status::Suspended, Denied::ShopSuspended),
    ] {
        s.set_status(status).await;
        refused(s.seller, denied).await;
    }
    s.set_status(shop::Status::Approved).await;
    let mut owner = user::Entity::find_by_id(s.seller)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    owner.status = Set(user::Status::Suspended);
    owner.update(&s.db).await.unwrap();
    refused(s.seller, Denied::Suspended).await;
}

/// FR-SHOP-06: "ตั้งค่าร้าน" shows the shop as its owner sees it, with how it is run, and a
/// button for each part that applies.
#[tokio::test]
async fn settings_offers_the_parts_that_apply() {
    let mut s = setup().await;
    let shown = s.taps(s.seller, "shop_settings").await;
    assert_eq!(shown.len(), 2);
    assert_eq!(row(&shown[0], "สถานะร้าน"), "เปิดอยู่");
    assert_eq!(row(&shown[0], "เวลาเปิด"), "ไม่ได้ระบุ");
    assert_eq!(row(&shown[0], "แสดงเบอร์"), "ผู้ซื้อที่มีออเดอร์เห็น");
    assert_eq!(row(&shown[0], "พร้อมเพย์"), "081-234-5678");
    assert_eq!(
        quick_replies(&shown[1]),
        [
            "ชื่อร้าน",
            "คำอธิบาย",
            "รูปร้าน",
            "เวลาเปิดร้าน",
            "ที่รับของ",
            "ตำแหน่งร้าน",
            "เบอร์ร้าน",
            "การรับส่ง",
            "ค่าส่ง",
            "ยอดขั้นต่ำ",
            "พร้อมเพย์",
            "ซ่อนเบอร์ร้าน",
            "ดูหน้าร้าน",
        ]
    );
    let data = quick_reply_data(&shown[1]);
    assert_eq!(data[0], Some("shop.part?p=name"));
    assert_eq!(data[11..], [Some("shop.phone?show=0"), Some("shop.front")]);
    assert_eq!(s.flow(s.seller).await, None);

    // Picked up only, without PromptPay: no fee, and no ID to set
    let mut pickup = s.shop().await.into_active_model();
    pickup.offers_delivery = Set(false);
    pickup.update(&s.db).await.unwrap();
    s.config.payment.methods = vec![PaymentMethod::Cash];
    let shown = s.taps(s.seller, "shop_settings").await;
    let parts = quick_replies(&shown[1]);
    assert!(
        !parts.contains(&"ค่าส่ง") && !parts.contains(&"พร้อมเพย์"),
        "{parts:?}"
    );
    assert_eq!(parts.len(), 11);
    // A button sent before then shows "ตั้งค่าร้าน" as it is
    let stale = s.taps(s.seller, "shop.part?p=delivery_fee").await;
    assert_eq!(stale, shown);
    assert_eq!(s.flow(s.seller).await, None);
    let unknown = s.taps(s.seller, "shop.part?p=summary").await;
    assert_eq!(unknown, shown);
}

/// FR-SHOP-06, FR-SHOP-07: each part is asked for with what it is now, and saved as soon as it
/// is answered; a change to delivering is saved with its fee.
#[tokio::test]
async fn each_part_is_saved_once_answered() {
    let s = setup().await;
    let seller = s.seller;
    let asked = s.taps(seller, "shop.part?p=name").await;
    assert!(
        text(&asked[0]).contains("(ตอนนี้: ร้านป้าแดง)"),
        "{}",
        text(&asked[0])
    );
    assert_eq!(quick_replies(&asked[0]), ["ใช้ของเดิม", "ยกเลิก"]);
    assert_eq!(s.flow(seller).await.as_deref(), Some(SettingsFlow::NAME));
    let saved = s.types(seller, "  ร้านป้าแดง   ไข่สด ").await;
    assert_eq!(saved.len(), 3);
    assert_eq!(saved[0], s.templates.shop_settings_saved());
    assert_eq!(card_texts(&saved[1])[0], "ร้านป้าแดง ไข่สด");
    assert_eq!(quick_replies(&saved[2])[0], "ชื่อร้าน");
    let shop = s.shop().await;
    assert_eq!(shop.name, "ร้านป้าแดง ไข่สด");
    assert_eq!(s.flow(seller).await, None);

    // Opening hours, set, refused when too long, then removed
    let asked = s.taps(seller, "shop.part?p=opening_hours").await;
    assert!(text(&asked[0]).contains("ไม่เกิน 50 ตัวอักษร"));
    assert_eq!(quick_replies(&asked[0]), ["ข้าม", "ยกเลิก"]);
    s.types(seller, "ทุกวัน 06:00–10:00").await;
    assert_eq!(
        s.shop().await.opening_hours.as_deref(),
        Some("ทุกวัน 06:00–10:00")
    );
    let asked = s.taps(seller, "shop.part?p=opening_hours").await;
    assert!(text(&asked[0]).contains("(ตอนนี้: ทุกวัน 06:00–10:00)"));
    assert_eq!(quick_replies(&asked[0]), ["ลบออก", "ใช้ของเดิม", "ยกเลิก"]);
    let refused = s.types(seller, &"ก".repeat(51)).await;
    assert_eq!(refused[0].body, s.templates.text_too_long(50).body);
    assert_eq!(s.flow(seller).await.as_deref(), Some(SettingsFlow::NAME));
    let saved = s.types(seller, "ลบออก").await;
    assert_eq!(row(&saved[1], "เวลาเปิด"), "ไม่ได้ระบุ");
    assert_eq!(s.shop().await.opening_hours, None);

    // Picked up only: saved at once, without a fee
    s.taps(seller, "shop.part?p=fulfillment").await;
    s.types(seller, "รับเองที่ร้าน").await;
    let shop = s.shop().await;
    assert!(shop.offers_pickup && !shop.offers_delivery);
    assert_eq!(shop.delivery_fee, Money::ZERO);
    // Delivered: nothing saved until the fee is answered
    s.taps(seller, "shop.part?p=fulfillment").await;
    let asked = s.types(seller, "ร้านไปส่ง").await;
    assert!(
        text(&asked[0]).starts_with("ค่าส่งครั้งละเท่าไร"),
        "{}",
        text(&asked[0])
    );
    assert!(!s.shop().await.offers_delivery);
    s.types(seller, "15 บาท").await;
    let shop = s.shop().await;
    assert!(!shop.offers_pickup && shop.offers_delivery);
    assert_eq!(shop.delivery_fee, Money::from_baht(15));

    s.taps(seller, "shop.part?p=minimum_order").await;
    s.types(seller, "50").await;
    assert_eq!(s.shop().await.minimum_order, Some(Money::from_baht(50)));

    // The phone, which the shop can't do without, is checked
    s.taps(seller, "shop.part?p=phone").await;
    let refused = s.types(seller, "12345").await;
    assert_eq!(refused[0].body, s.templates.shop_phone_invalid().body);
    assert_eq!(s.shop().await.phone.as_deref(), Some("0812345678"));
    s.types(seller, "089-876-5432").await;
    assert_eq!(s.shop().await.phone.as_deref(), Some("0898765432"));

    // "ใช้ของเดิม" writes nothing
    let before = s.shop().await;
    s.taps(seller, "shop.part?p=description").await;
    let kept = s.handle_at(seller, t0() + TimeDelta::minutes(5), async |turn| {
        let session = turn.flow.as_ref().and_then(|flow| flow.read()).unwrap();
        let line = s.line.client();
        let input = Input::Text("ใช้ของเดิม".to_owned());
        Ok(answer(turn, &s.config, &line, session, &input)
            .await?
            .unwrap())
    });
    let kept = kept.await;
    assert_eq!(kept[0], s.templates.shop_settings_unchanged());
    assert_eq!(s.shop().await, before);
    assert_eq!(s.flow(seller).await, None);
    // And a photo where text is asked for goes on by FR-UI-14
    s.taps(seller, "shop.part?p=name").await;
    let photo = Input::Image {
        message_id: "1".to_owned(),
    };
    assert_eq!(s.sends(seller, photo).await, None);
}

/// FR-SHOP-06: a new photo replaces the shop's, and "ลบออก" removes it.
#[tokio::test]
async fn a_new_photo_replaces_the_old() {
    let s = setup().await;
    let seller = s.seller;
    let asked = s.taps(seller, "shop.part?p=photo").await;
    assert_eq!(
        quick_replies(&asked[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    let saved = s.sends_photo(seller, "m1", PHOTO).await;
    assert_eq!(saved[0], s.templates.shop_settings_saved());
    let [(OwnerType::Shop, owner, first)] = s.images().await[..] else {
        panic!("one photo of the shop's");
    };
    assert_eq!(owner, s.shop);
    assert!(
        hero(&saved[1])
            .unwrap()
            .ends_with(&format!("/img/p/{first}"))
    );

    let asked = s.taps(seller, "shop.part?p=photo").await;
    assert!(quick_replies(&asked[0]).contains(&"ลบออก"));
    s.sends_photo(seller, "m2", PHOTO).await;
    let [(OwnerType::Shop, _, second)] = s.images().await[..] else {
        panic!("one photo of the shop's");
    };
    assert_ne!(second, first);

    s.taps(seller, "shop.part?p=photo").await;
    let removed = s.types(seller, "ลบออก").await;
    assert_eq!(hero(&removed[1]), None);
    assert_eq!(s.images().await, []);
}

/// FR-UI-04, FR-UI-15: cancelling a part, or letting it expire, changes nothing.
#[tokio::test]
async fn cancelling_or_expiring_changes_nothing() {
    let s = setup().await;
    let before = s.shop().await;
    s.taps(s.seller, "shop.part?p=name").await;
    s.handle_at(s.seller, t0(), async |turn| flow::cancel(turn).await)
        .await;
    assert_eq!(s.flow(s.seller).await, None);

    s.taps(s.seller, "shop.part?p=photo").await;
    s.sends_photo(s.seller, "m1", PHOTO).await;
    // The photo is saved at once; a part left unanswered expires
    let photo = s.images().await;
    s.taps(s.seller, "shop.part?p=description").await;
    let expired = t0() + TimeDelta::minutes(16);
    s.handle_at(s.seller, expired, async |turn| {
        assert!(turn.flow.is_none());
        Ok(Vec::new())
    })
    .await;
    assert_eq!(s.flow(s.seller).await, None);
    let shop = s.shop().await;
    assert_eq!(
        (shop.name, shop.description),
        (before.name, before.description)
    );
    assert_eq!(s.images().await, photo);
}

/// FR-SHOP-08: the shop's phone is shown to buyers with an active order unless hidden, at a tap
/// that names what it sets.
#[tokio::test]
async fn the_phone_can_be_hidden_from_buyers() {
    let s = setup().await;
    assert!(s.shop().await.show_phone);
    let hidden = s.taps(s.seller, "shop.phone?show=0").await;
    assert_eq!(hidden.len(), 3);
    assert_eq!(hidden[0], s.templates.phone_visibility(false));
    assert_eq!(row(&hidden[1], "แสดงเบอร์"), "ซ่อนจากผู้ซื้อ");
    assert!(quick_replies(&hidden[2]).contains(&"แสดงเบอร์ร้าน"));
    assert!(!s.shop().await.show_phone);

    let again = s
        .taps_at(s.seller, "shop.phone?show=0", t0() + TimeDelta::hours(1))
        .await;
    assert_eq!(again, hidden);
    assert_eq!(s.shop().await.updated_at, t0());

    let shown = s.taps(s.seller, "shop.phone?show=1").await;
    assert_eq!(shown[0], s.templates.phone_visibility(true));
    assert!(s.shop().await.show_phone);
}

/// NFR-SEC-10: a suspension while a part is being changed ends it, saving nothing.
#[tokio::test]
async fn a_suspension_mid_settings_ends_it() {
    let s = setup().await;
    s.taps(s.seller, "shop.part?p=name").await;
    s.set_status(shop::Status::Suspended).await;
    let reply = s.types(s.seller, "ร้านใหม่").await;
    assert_eq!(reply, [s.templates.denied(Denied::ShopSuspended)]);
    assert_eq!(s.shop().await.name, "ร้านป้าแดง");
    assert_eq!(s.flow(s.seller).await, None);
}

/// FR-SHOP-09, NFR-PRIV-04: the card buyers see shows the photo, name, whether the shop is open,
/// its description, how it sells, its minimum and its hours, with "ดูสินค้า"; but none of what
/// only its owner sees.
#[tokio::test]
async fn the_shop_front_shows_what_buyers_see() {
    let s = setup().await;
    let mut shop = s.shop().await.into_active_model();
    shop.opening_hours = Set(Some("ทุกวัน 06:00–10:00".to_owned()));
    shop.minimum_order = Set(Some(Money::from_baht(50)));
    shop.update(&s.db).await.unwrap();

    let front = s.taps(s.seller, "shop.front").await;
    assert_eq!(front.len(), 2);
    assert_eq!(text(&front[0]), "ผู้ซื้อจะเห็นร้านของคุณแบบนี้ค่ะ");
    assert_eq!(
        card_texts(&front[1]),
        [
            "ร้านป้าแดง",
            "เปิดอยู่",
            "ไข่ไก่ ผักสวนครัว",
            "การรับส่ง",
            "รับเองหรือให้ร้านไปส่ง ค่าส่ง ฿10",
            "ยอดขั้นต่ำ",
            "฿50",
            "เวลาเปิด",
            "ทุกวัน 06:00–10:00",
        ]
    );
    assert_eq!(buttons(&front[1]), [format!("browse.shop?shop={}", s.shop)]);
    assert_eq!(hero(&front[1]), None);
    assert_eq!(quick_replies(&front[1]), ["ตั้งค่าร้าน", "เมนู"]);
    let footer = &bubble(&front[1]).footer.as_ref().unwrap().contents;
    let Component::Button(button) = &footer[0] else {
        panic!("not a button");
    };
    assert_eq!(button.action.label(), &labels::VIEW_PRODUCTS);

    s.taps(s.seller, "open_close").await;
    let front = s.taps(s.seller, "shop.front").await;
    assert_eq!(card_texts(&front[1])[1], "ร้านปิด");
}
