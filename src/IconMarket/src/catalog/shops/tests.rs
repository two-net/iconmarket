//! Shop applications (FR-SHOP-01 – FR-SHOP-04): the flow's steps and what they refuse, the
//! address asked for first, the photo kept as a draft until the application is sent, the
//! application sent for an admin or opened at once, a second application refused, and
//! applying again once turned down. Each event is handled in a transaction committed once it
//! is handled, with what it notifies queued in it. The `async` tests each start a throwaway
//! PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{DatabaseConnection, IntoActiveModel, QueryOrder};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer, Hero};
use crate::line::testing::{self as fake, Fake};
use crate::line::{Body, limits};
use crate::notifications::templates::{Reported, Templates};
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{audit_log, push_log, report, session};
use crate::persistence::{orm, testing};
use crate::users::address::AddressFlow;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const APPLICANT: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const ADMIN: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

#[test]
fn a_first_application_asks_every_part_in_order() {
    let config = config::sample();
    let mut draft = ShopFlow::new();
    let mut step = Step::Name;
    let mut asked = vec![step];
    while step != Step::Summary {
        // What answering the step leaves, as far as the order goes
        match step {
            Step::Name => draft.name = Some("ร้านป้าแดง".to_owned()),
            Step::Description => draft.description = Some("ไข่ไก่".to_owned()),
            Step::PickupAddress => draft.pickup_address = Some("ซอย 3".to_owned()),
            Step::Phone => draft.phone = Some("0812345678".to_owned()),
            Step::Fulfillment => draft.fulfillment = Some(Fulfillment::Both),
            _ => {}
        }
        step = after(&config, &draft, step);
        asked.push(step);
    }
    assert_eq!(
        asked,
        [
            Step::Name,
            Step::Description,
            Step::Photo,
            Step::PickupAddress,
            Step::PickupLocation,
            Step::Phone,
            Step::Fulfillment,
            Step::DeliveryFee,
            Step::MinimumOrder,
            Step::PromptPay,
            Step::Summary,
        ]
    );

    // A shop that doesn't deliver has no fee, and without PromptPay there is no ID to ask
    let mut config = config::sample();
    config.payment.methods = vec![PaymentMethod::Cash];
    draft.fulfillment = Some(Fulfillment::Pickup);
    assert_eq!(
        after(&config, &draft, Step::Fulfillment),
        Step::MinimumOrder
    );
    assert_eq!(after(&config, &draft, Step::MinimumOrder), Step::Summary);

    // Correcting a part goes back to the summary, by the fee if delivery is now offered
    draft.editing = true;
    assert_eq!(after(&config, &draft, Step::Name), Step::Summary);
    assert_eq!(after(&config, &draft, Step::Fulfillment), Step::Summary);
    draft.fulfillment = Some(Fulfillment::Delivery);
    assert_eq!(after(&config, &draft, Step::Fulfillment), Step::DeliveryFee);
    // Never to a summary missing what the shop can't do without
    draft.phone = None;
    assert_eq!(after(&config, &draft, Step::Name), Step::Phone);
}

#[test]
fn every_part_has_its_own_postback_and_label() {
    for part in Step::PARTS {
        assert_eq!(Step::part(part.key()), Some(part));
        let data = Postback::new(PART)
            .with_name(WHICH, part.key())
            .with_int(REVISION, i64::MAX)
            .to_string();
        assert!(data.len() <= limits::ACTION_TEXT_CHARS, "{data}");
        assert_eq!(
            Postback::parse(&data).unwrap().name(WHICH),
            Some(part.key())
        );
    }
    assert_eq!(Step::part("summary"), None);
    let labels: Vec<_> = Step::PARTS.iter().map(|part| part.label()).collect();
    for (i, label) in labels.iter().enumerate() {
        assert!(!labels[..i].contains(label), "{label:?}");
    }
    for (fulfillment, label) in Fulfillment::ALL {
        assert_eq!(fulfillment.label(), label);
        let (pickup, delivery) = (fulfillment.pickup(), fulfillment.delivery());
        assert_eq!(Fulfillment::of(pickup, delivery), Some(fulfillment));
    }
    assert_eq!(Fulfillment::of(false, false), None);
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    /// Off, unless a test scripts it
    ai: ai::Gateway,
    templates: Templates,
    /// A user with an address and a phone, but no shared location; an admin, whose LINE user
    /// ID is [`ADMIN`], is alerted of applications
    applicant: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let applicant = user::ActiveModel {
        display_name: Set(Some("ป้าแดง".to_owned())),
        zone: Set(Some("ซอย 3".to_owned())),
        house_number: Set(Some("45/2".to_owned())),
        phone: Set(Some("0812345678".to_owned())),
        ..testing::new_user(APPLICANT)
    };
    let applicant = applicant.insert(&db).await.unwrap().id;
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(ADMIN)
    };
    admin.insert(&db).await.unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        ai: ai::testing::off(),
        applicant,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s with `handle`, as the worker does: in a transaction
    /// that commits once it is handled, with the user loaded afresh and what it notifies queued.
    /// Returns the reply and the users whose rich menus to relink.
    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> (Vec<Message>, Vec<i64>) {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages,
            outbox,
            relink,
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
        (messages, relink)
    }

    /// User `user_id` taps a button sending `data`.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.tapped(user_id, data).await.0
    }

    /// User `user_id` taps a button sending `data`: the reply, and the menus to relink.
    async fn tapped(&self, user_id: i64, data: &str) -> (Vec<Message>, Vec<i64>) {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            tapped(turn, &self.config, &self.ai, &postback).await
        })
        .await
    }

    /// User `user_id` answers their application with `input`; `None` if the step doesn't take
    /// it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let line = self.line.client();
        let mut answered = None;
        self.handle(user_id, async |turn| {
            let session = turn.flow.as_ref().and_then(|flow| flow.read::<ShopFlow>());
            let session = session.expect("applying for a shop");
            answered = answer(
                turn,
                &self.config,
                &ai::testing::off(),
                &line,
                session,
                &input,
            )
            .await?;
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

    /// User `user_id` answers their address flow with `text`.
    async fn types_address(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        let (messages, _) = self
            .handle(user_id, async |turn| {
                let session = turn
                    .flow
                    .as_ref()
                    .and_then(|flow| flow.read::<AddressFlow>());
                let session = session.expect("giving the address");
                let answered = address::answer(turn, &self.config, session, &input).await?;
                Ok(answered.expect("an answer"))
            })
            .await;
        messages
    }

    /// User `user_id`'s application in progress, if any.
    async fn session(&self, user_id: i64) -> Option<Session<ShopFlow>> {
        let row = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()?;
        if row.flow != ShopFlow::NAME {
            return None;
        }
        Some(Session {
            step: serde_json::from_value(Json::String(row.step)).unwrap(),
            draft: serde_json::from_value(row.data).unwrap(),
        })
    }

    async fn flow(&self, user_id: i64) -> Option<String> {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .map(|row| row.flow)
    }

    async fn revision(&self, user_id: i64) -> i64 {
        self.session(user_id)
            .await
            .expect("applying")
            .draft
            .revision
    }

    /// User `user_id` taps the latest summary's button for `action`.
    async fn taps_latest(&self, user_id: i64, action: &str) -> (Vec<Message>, Vec<i64>) {
        let revision = self.revision(user_id).await;
        self.tapped(user_id, &format!("{action}?r={revision}"))
            .await
    }

    /// User `user_id` taps the latest summary's button correcting `part`.
    async fn corrects(&self, user_id: i64, part: Step) -> Vec<Message> {
        let revision = self.revision(user_id).await;
        let data = format!("{PART}?p={}&r={revision}", part.key());
        self.taps(user_id, &data).await
    }

    /// User `user_id` applies, from "เปิดร้าน" to the summary, as Appendix A.5's shop: picked
    /// up at home or delivered for ฿10, without a photo, location, minimum or PromptPay.
    async fn fill(&self, user_id: i64) -> Vec<Message> {
        self.fill_as(user_id, "ไข่ไก่ ผักสวนครัว").await
    }

    /// User `user_id` applies likewise, but with `description`.
    async fn fill_as(&self, user_id: i64, description: &str) -> Vec<Message> {
        self.taps(user_id, "my_shop").await;
        for answer in [
            "ร้านป้าแดง",
            description,
            "ข้าม",
            "ใช้ที่อยู่บ้าน",
            "ข้าม",
            "081-234-5678",
            "ได้ทั้งสองแบบ",
            "10",
            "ไม่มีขั้นต่ำ",
        ] {
            self.types(user_id, answer).await;
        }
        self.types(user_id, "ข้าม").await
    }

    async fn shops(&self) -> Vec<shop::Model> {
        shop::Entity::find()
            .order_by_asc(shop::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn user(&self, user_id: i64) -> user::Model {
        user::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
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

    /// Every audit entry, as (actor, action, target, details).
    async fn audited(&self) -> Vec<(Option<i64>, String, Option<i64>, Json)> {
        audit_log::Entity::find()
            .order_by_asc(audit_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.actor_id, row.action, row.target_id, row.details))
            .collect()
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

    async fn main_menu(&self, user_id: i64) -> Message {
        let actor = Actor::load(&self.db, user_id).await.unwrap().unwrap();
        menu::main_menu(&actor)
    }

    /// A shop of user `owner_id`'s, `status`, as an earlier application left it.
    async fn new_shop(&self, owner_id: i64, status: shop::Status) -> shop::Model {
        let shop = shop::ActiveModel {
            status: Set(status),
            phone: Set(Some("0812345678".to_owned())),
            pickup_address: Set(Some("ซอย 3 บ้านเลขที่ 45/2".to_owned())),
            ..testing::new_shop(owner_id)
        };
        shop.insert(&self.db).await.unwrap()
    }
}

/// The messages of one push, as queued.
fn queued(messages: &[Message]) -> Option<Json> {
    Some(serde_json::to_value(messages).unwrap())
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

/// Acceptance (FR-SHOP-01, FR-SHOP-03): every part is asked for with its choices, nothing is
/// saved until the summary's "ส่งคำขอ", and the application then waits as `PENDING` while the
/// admins are alerted with its card, as in Appendix A.5.
#[tokio::test]
async fn an_application_is_sent_for_an_admin_to_approve() {
    let s = setup().await;
    let a = s.applicant;
    let asked = |messages: &[Message]| {
        assert_eq!(messages.len(), 1, "{messages:?}");
        (
            text(&messages[0]).to_owned(),
            quick_replies(&messages[0]).join(" · "),
        )
    };

    let (name, choices) = asked(&s.taps(a, "my_shop").await);
    assert!(name.starts_with("มาเปิดร้านกันค่ะ") && name.contains("ไม่เกิน 50 ตัวอักษร"));
    assert_eq!(choices, "ยกเลิก");
    let (description, _) = asked(&s.types(a, "  ร้านป้าแดง ").await);
    assert!(description.contains("ไม่เกิน 300 ตัวอักษร"), "{description}");
    let (photo, choices) = asked(&s.types(a, "ไข่ไก่ ผักสวนครัว").await);
    assert!(photo.contains("กด \"ข้าม\""), "{photo}");
    assert_eq!(choices, "ถ่ายรูป · เลือกรูป · ข้าม · ยกเลิก");
    let (pickup, choices) = asked(&s.types(a, "ข้าม").await);
    assert!(pickup.contains("ไม่เกิน 200 ตัวอักษร"), "{pickup}");
    assert_eq!(choices, "ใช้ที่อยู่บ้าน · ข้าม · ยกเลิก");
    let (location, choices) = asked(&s.types(a, "ใช้ที่อยู่บ้าน").await);
    assert!(location.contains("กด \"แชร์ตำแหน่ง\""), "{location}");
    assert_eq!(choices, "แชร์ตำแหน่ง · ข้าม · ยกเลิก");
    let (phone, choices) = asked(&s.types(a, "ข้าม").await);
    assert!(phone.contains("กดเบอร์ด้านล่าง"), "{phone}");
    assert_eq!(choices, "081-234-5678 · ยกเลิก");
    let (_, choices) = asked(&s.types(a, "081-234-5678").await);
    assert_eq!(choices, "รับเองที่ร้าน · ร้านไปส่ง · ได้ทั้งสองแบบ · ยกเลิก");
    let (fee, choices) = asked(&s.types(a, "ได้ทั้งสองแบบ").await);
    assert!(fee.contains("ไม่เกิน ฿1,000"), "{fee}");
    assert_eq!(choices, "ไม่มีค่าส่ง · ยกเลิก");
    let (minimum, choices) = asked(&s.types(a, "10 บาท").await);
    assert!(minimum.contains("ไม่เกิน ฿100,000"), "{minimum}");
    assert_eq!(choices, "ไม่มีขั้นต่ำ · ยกเลิก");
    let (promptpay, choices) = asked(&s.types(a, "ไม่มีขั้นต่ำ").await);
    assert!(promptpay.contains("หรือกดเบอร์ร้านด้านล่าง"), "{promptpay}");
    assert_eq!(choices, "081-234-5678 · ข้าม · ยกเลิก");
    // Nothing is saved before it is sent
    assert_eq!(s.shops().await, []);

    let summary = s.types(a, "ข้าม").await;
    assert_eq!(summary.len(), 2);
    assert!(text(&summary[0]).contains("กด \"ส่งคำขอ\" แล้วผู้ดูแลจะตรวจก่อนเปิดร้าน"));
    let card = &summary[1];
    assert_eq!(
        buttons(card),
        ["open_shop.submit?r=10", "open_shop.edit?r=10"]
    );
    assert_eq!(quick_replies(card), ["ยกเลิก"]);
    assert_eq!(hero(card), None);
    let texts = card_texts(card);
    for shown in [
        "ร้านป้าแดง",
        "ไข่ไก่ ผักสวนครัว",
        "ซอย 3 บ้านเลขที่ 45/2",
        "ไม่ได้แชร์",
        "081-234-5678",
        "รับเองหรือให้ร้านไปส่ง ค่าส่ง ฿10",
    ] {
        assert!(
            texts.iter().any(|text| text == shown),
            "{shown} in {texts:?}"
        );
    }

    let (sent, relink) = s.taps_latest(a, SUBMIT).await;
    assert_eq!(
        sent,
        [
            s.templates.application_sent("ร้านป้าแดง"),
            s.main_menu(a).await
        ]
    );
    assert!(relink.is_empty(), "{relink:?}");
    assert_eq!(s.flow(a).await, None);
    let shops = s.shops().await;
    assert_eq!(shops.len(), 1);
    let shop = &shops[0];
    assert_eq!(
        (
            shop.owner_id,
            shop.name.as_str(),
            shop.description.as_str(),
            shop.status
        ),
        (a, "ร้านป้าแดง", "ไข่ไก่ ผักสวนครัว", shop::Status::Pending)
    );
    assert_eq!(
        (
            shop.offers_pickup,
            shop.offers_delivery,
            shop.delivery_fee,
            shop.minimum_order
        ),
        (true, true, Money::from_baht(10), None)
    );
    assert_eq!(
        (
            shop.pickup_address.as_deref(),
            shop.pickup_latitude,
            shop.phone.as_deref(),
            shop.promptpay_id.as_deref(),
        ),
        (Some("ซอย 3 บ้านเลขที่ 45/2"), None, Some("0812345678"), None)
    );
    assert_eq!(
        (
            shop.is_open,
            shop.show_phone,
            shop.rejection_reason.as_deref()
        ),
        (true, true, None)
    );
    assert_eq!((shop.created_at, shop.updated_at), (t0(), t0()));

    let alert = approvals::alert(&s.templates, shop, &s.user(a).await, None);
    assert_eq!(
        buttons(&alert),
        [
            format!(
                "approvals.approve?s={}&v={}",
                shop.id,
                approvals::version(shop)
            ),
            format!(
                "approvals.reject?s={}&v={}",
                shop.id,
                approvals::version(shop)
            ),
        ]
    );
    assert_eq!(
        s.pushes().await,
        [(
            "admin.shop_application".to_owned(),
            ADMIN.to_owned(),
            queued(&[alert])
        )]
    );
    assert_eq!(s.audited().await, []);
}

/// FR-SHOP-03: with `shop.requireApproval` off, the shop opens as it is sent, and its owner
/// gets the seller's menu at once; the system's approval is audited, and no admin alerted.
#[tokio::test]
async fn without_approval_the_shop_opens_at_once() {
    let mut s = setup().await;
    s.config.shop.require_approval = false;
    let a = s.applicant;
    let summary = s.fill(a).await;
    assert!(text(&summary[0]).contains("กด \"เปิดร้านเลย\" แล้วร้านจะเปิดทันที"));

    let (opened, relink) = s.taps_latest(a, SUBMIT).await;
    let menu = s.main_menu(a).await;
    assert_eq!(opened, [s.templates.shop_opened("ร้านป้าแดง"), menu.clone()]);
    assert!(
        serde_json::to_string(&menu).unwrap().contains("ออเดอร์ร้าน"),
        "the seller's functions"
    );
    assert_eq!(relink, [a]);
    let shop = &s.shops().await[0];
    assert_eq!(shop.status, shop::Status::Approved);
    assert_eq!(
        s.audited().await,
        [(
            None,
            "shop.approved".to_owned(),
            Some(shop.id),
            json!({ "name": "ร้านป้าแดง", "reason": "shop.requireApproval is false" })
        )]
    );
    assert_eq!(s.pushes().await, []);
}

/// FR-PROD-09: an application moderation finds suspicious waits for an admin, even with
/// `shop.requireApproval` off. The admins are alerted with its card and why, and the hold is
/// audited; no report is filed, as approving the application is its review.
#[tokio::test]
async fn a_suspicious_application_waits_for_an_admin() {
    let mut s = setup().await;
    s.config.shop.require_approval = false;
    s.config.catalog.prohibited_keywords = vec!["บุหรี่ไฟฟ้า".to_owned()];
    let a = s.applicant;
    s.fill_as(a, "ไข่ไก่ มีบุหรี่ไฟฟ้าด้วย").await;

    let (sent, relink) = s.taps_latest(a, SUBMIT).await;
    let menu = s.main_menu(a).await;
    assert_eq!(sent, [s.templates.application_sent("ร้านป้าแดง"), menu]);
    assert!(relink.is_empty(), "no seller's menu yet");
    let shop = &s.shops().await[0];
    assert_eq!(shop.status, shop::Status::Pending);

    let reason = "มีคำต้องห้าม \"บุหรี่ไฟฟ้า\"";
    let application = approvals::alert(&s.templates, shop, &s.user(a).await, None);
    let approvals = Item::ApproveShops
        .postback()
        .button(labels::APPROVE_SHOPS.clone());
    let what = Reported::Shop {
        name: "ร้านป้าแดง"
    };
    let held = s.templates.held_alert(&what, reason, t0(), vec![approvals]);
    assert_eq!(
        s.pushes().await,
        [(
            "admin.shop_application,admin.held_for_review".to_owned(),
            ADMIN.to_owned(),
            queued(&[application, held])
        )]
    );
    assert_eq!(
        s.audited().await,
        [(
            None,
            "moderation.held".to_owned(),
            Some(shop.id),
            json!({ "name": "ร้านป้าแดง", "reason": reason })
        )]
    );
    let reports = report::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(reports, []);
}

/// FR-ONB-04: a user without an address gives it first, then goes straight on to the
/// application.
#[tokio::test]
async fn the_address_is_asked_for_first() {
    let s = setup().await;
    let user = testing::new_user("U-no-address")
        .insert(&s.db)
        .await
        .unwrap();
    let zone = s.taps(user.id, "my_shop").await;
    assert!(text(&zone[0]).starts_with("ขอที่อยู่สำหรับส่งของก่อนนะคะ"));
    assert_eq!(s.flow(user.id).await.as_deref(), Some(AddressFlow::NAME));
    for answer in ["ซอย 5", "12", "ข้าม", "ข้าม"] {
        s.types_address(user.id, answer).await;
    }
    let resumed = s.types_address(user.id, "ข้าม").await;
    assert_eq!(resumed[0], s.templates.address_saved("ซอย 5 บ้านเลขที่ 12"));
    assert!(text(&resumed[1]).starts_with("มาเปิดร้านกันค่ะ"));
    let session = s.session(user.id).await.expect("applying");
    assert_eq!(session.step, Step::Name);
    assert_eq!(s.user(user.id).await.zone.as_deref(), Some("ซอย 5"));
}

/// FR-SHOP-01: each step says what it takes when given something else, and asks again with
/// its choices; a photo or a location where neither is asked for goes on to routing.
#[tokio::test]
async fn each_step_refuses_what_it_cannot_take() {
    let s = setup().await;
    let a = s.applicant;
    let t = &s.templates;
    s.taps(a, "my_shop").await;
    let at = async |step| s.session(a).await.unwrap().step == step;

    let too_long = "ก".repeat(NAME_CHARS + 1);
    assert_eq!(
        s.types(a, &too_long).await,
        [templates::with_cancel(t.text_too_long(NAME_CHARS))]
    );
    assert!(text(&s.types(a, "  ").await[0]).contains("ร้านชื่ออะไรดีคะ"));
    let elsewhere = Input::Location(Location {
        latitude: 16.44,
        longitude: 102.83,
        address: None,
    });
    assert_eq!(s.sends(a, elsewhere.clone()).await, None);
    let photo = Input::Image {
        message_id: "m0".to_owned(),
    };
    assert_eq!(s.sends(a, photo).await, None);
    assert!(at(Step::Name).await);
    let longest = "ข".repeat(NAME_CHARS);
    s.types(a, &longest).await;

    let too_long = "ก".repeat(DESCRIPTION_CHARS + 1);
    assert_eq!(
        s.types(a, &too_long).await,
        [templates::with_cancel(t.text_too_long(DESCRIPTION_CHARS))]
    );
    s.types(a, "ไข่ไก่").await;

    let refused = s.types(a, "ไม่มีรูป").await;
    assert_eq!(text(&refused[0]), text(&t.photo_expected(&labels::SKIP)));
    assert_eq!(
        quick_replies(&refused[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    let not_a_photo = s.sends_photo(a, "m1", b"not a photo").await;
    assert_eq!(
        text(&not_a_photo[0]),
        t.image_rejected(Rejected::NotJpegOrPng)
    );
    s.types(a, "ข้าม").await;

    let too_long = "ก".repeat(PICKUP_ADDRESS_CHARS + 1);
    assert_eq!(
        text(&s.types(a, &too_long).await[0]),
        text(&t.text_too_long(PICKUP_ADDRESS_CHARS))
    );
    // Without an address, the location is needed
    let location = s.types(a, "ข้าม").await;
    assert_eq!(quick_replies(&location[0]), ["แชร์ตำแหน่ง", "ยกเลิก"]);
    assert_eq!(text(&s.types(a, "ข้าม").await[0]), text(&t.pickup_needed()));
    assert_eq!(
        text(&s.types(a, "หน้าวัด").await[0]),
        text(&t.shop_location_expected(None))
    );
    s.sends(a, elsewhere).await.expect("taken");
    assert!(at(Step::Phone).await);

    assert_eq!(
        text(&s.types(a, "123").await[0]),
        text(&t.shop_phone_invalid())
    );
    s.types(a, "๐๘๙ ๙๙๙ ๙๙๙๙").await;

    let again = s.types(a, "อะไรก็ได้").await;
    assert!(text(&again[0]).contains("ลูกค้ารับของได้แบบไหนบ้าง"));
    s.types(a, "ร้านไปส่ง").await;

    assert_eq!(
        text(&s.types(a, "สิบบาท").await[0]),
        t.number_problem(NumberError::NotANumber)
    );
    assert_eq!(
        text(&s.types(a, "1,000.01").await[0]),
        text(&t.amount_too_large(MAX_DELIVERY_FEE))
    );
    s.types(a, "๑๕.๕๐ บาท").await;

    assert_eq!(
        text(&s.types(a, "100,001").await[0]),
        text(&t.amount_too_large(MAX_MINIMUM_ORDER))
    );
    s.types(a, "0").await;

    let refused = s.types(a, "1234").await;
    assert_eq!(text(&refused[0]), text(&t.promptpay_invalid(&labels::SKIP)));
    assert_eq!(quick_replies(&refused[0]), ["089-999-9999", "ข้าม", "ยกเลิก"]);
    s.types(a, "1-1017-00203-45-0").await;

    let Session { step, draft } = s.session(a).await.unwrap();
    assert_eq!(step, Step::Summary);
    assert_eq!(
        (
            draft.name.as_deref(),
            draft.pickup_address.as_deref(),
            draft.pickup_location,
            draft.phone.as_deref(),
        ),
        (
            Some(longest.as_str()),
            None,
            Some(Point {
                latitude: 16.44,
                longitude: 102.83
            }),
            Some("0899999999"),
        )
    );
    assert_eq!(
        (
            draft.fulfillment,
            draft.delivery_fee,
            draft.minimum_order,
            draft.promptpay_id.as_deref()
        ),
        (
            Some(Fulfillment::Delivery),
            Money::from_satang(1_550),
            None,
            Some("1101700203450")
        )
    );
    // Text at the summary shows it again
    assert_eq!(s.types(a, "ส่งเลย").await.len(), 2);
    assert_eq!(s.shops().await, []);
}

/// The fee is asked only of a shop that delivers, and the PromptPay ID only while buyers can
/// pay by PromptPay; nor does the summary offer to correct them then.
#[tokio::test]
async fn steps_that_do_not_apply_are_skipped() {
    let mut s = setup().await;
    s.config.payment.methods = vec![PaymentMethod::Cash];
    let a = s.applicant;
    s.taps(a, "my_shop").await;
    for answer in ["ร้านป้าแดง", "ไข่ไก่", "ข้าม", "ใช้ที่อยู่บ้าน", "ข้าม", "081-234-5678"]
    {
        s.types(a, answer).await;
    }
    let minimum = s.types(a, "รับเองที่ร้าน").await;
    assert!(text(&minimum[0]).contains("ขั้นต่ำ"));
    let summary = s.types(a, "50").await;
    assert_eq!(summary.len(), 2);
    let texts = card_texts(&summary[1]);
    assert!(texts.iter().any(|text| text == "รับเองที่ร้าน"), "{texts:?}");
    assert!(texts.iter().any(|text| text == "฿50"), "{texts:?}");

    let parts = s.taps_latest(a, EDIT).await.0;
    assert_eq!(
        quick_replies(&parts[0]),
        [
            "ชื่อร้าน",
            "คำอธิบาย",
            "รูปร้าน",
            "ที่รับของ",
            "ตำแหน่งร้าน",
            "เบอร์ร้าน",
            "การรับส่ง",
            "ยอดขั้นต่ำ",
            "ยกเลิก"
        ]
    );
    let revision = s.revision(a).await;
    let fee = format!("{PART}?p=delivery_fee&r={revision}");
    assert_eq!(s.taps(a, &fee).await, summary);
}

/// A photo is downloaded as it is sent and kept as the user's draft: a second replaces it,
/// the summary shows it, and the shop adopts it as its own once the application is sent.
#[tokio::test]
async fn the_photo_is_a_draft_until_the_application_is_sent() {
    let s = setup().await;
    let a = s.applicant;
    s.taps(a, "my_shop").await;
    s.types(a, "ร้านป้าแดง").await;
    s.types(a, "ไข่ไก่").await;
    let next = s.sends_photo(a, "m1", PHOTO).await;
    assert!(text(&next[0]).contains("ลูกค้ามารับของที่ไหน"));
    let [(OwnerType::Draft, owner, first)] = s.images().await[..] else {
        panic!("one draft: {:?}", s.images().await);
    };
    assert_eq!(owner, a);
    for answer in ["ใช้ที่อยู่บ้าน", "ข้าม", "081-234-5678", "รับเองที่ร้าน", "ไม่มีขั้นต่ำ"]
    {
        s.types(a, answer).await;
    }
    let summary = s.types(a, "ข้าม").await;
    let base = &s.config.app.base_url;
    let url = |id| images::product_image_url(base, id).to_string();
    assert_eq!(hero(&summary[1]), Some(url(first).as_str()));

    // Correcting the photo: the one sent before goes
    let ask = s.corrects(a, Step::Photo).await;
    assert_eq!(
        quick_replies(&ask[0]),
        ["ถ่ายรูป", "เลือกรูป", "ลบออก", "ใช้ของเดิม", "ยกเลิก"]
    );
    let summary = s.sends_photo(a, "m2", PHOTO).await;
    let [(OwnerType::Draft, _, second)] = s.images().await[..] else {
        panic!("one draft: {:?}", s.images().await);
    };
    assert_ne!(first, second);
    assert_eq!(hero(&summary[1]), Some(url(second).as_str()));

    s.taps_latest(a, SUBMIT).await;
    let shop = &s.shops().await[0];
    assert_eq!(s.images().await, [(OwnerType::Shop, shop.id, second)]);
    let alert = &s.pushes().await[0].2;
    assert!(
        alert.as_ref().unwrap().to_string().contains(&url(second)),
        "the admins see the photo"
    );
}

/// A photo sent but swept before the application is sent, the flow having been left too long,
/// is asked for again; nothing is saved meanwhile.
#[tokio::test]
async fn a_lost_photo_is_asked_for_again() {
    let s = setup().await;
    let a = s.applicant;
    s.taps(a, "my_shop").await;
    s.types(a, "ร้านป้าแดง").await;
    s.types(a, "ไข่ไก่").await;
    s.sends_photo(a, "m1", PHOTO).await;
    for answer in [
        "ใช้ที่อยู่บ้าน",
        "ข้าม",
        "081-234-5678",
        "รับเองที่ร้าน",
        "ไม่มีขั้นต่ำ",
        "ข้าม",
    ] {
        s.types(a, answer).await;
    }
    image::Entity::delete_many().exec(&s.db).await.unwrap();

    let lost = s.taps_latest(a, SUBMIT).await.0;
    assert_eq!(lost[0], s.templates.photo_lost());
    assert!(text(&lost[1]).contains("มีรูปหน้าร้าน"));
    assert_eq!(s.shops().await, []);
    assert_eq!(s.pushes().await, []);
    let session = s.session(a).await.unwrap();
    assert_eq!((session.step, session.draft.photo), (Step::Photo, None));
    // Going without, it is sent
    s.types(a, "ข้าม").await;
    s.taps_latest(a, SUBMIT).await;
    assert_eq!(s.shops().await.len(), 1);
}

/// Acceptance (FR-SHOP-02): a user who has a shop, approved or awaiting an admin, is shown it
/// instead of a new application; one who got a shop while applying can't send theirs.
#[tokio::test]
async fn a_second_application_is_refused() {
    let s = setup().await;
    let a = s.applicant;
    let pending = s.new_shop(a, shop::Status::Pending).await;
    let shown = s.taps(a, "my_shop").await;
    assert_eq!(
        shown[1],
        s.templates
            .application_pending("ร้านป้าแดง", pending.updated_at)
    );
    assert!(text(&shown[1]).contains("จึงยังส่งคำขอใหม่ไม่ได้"));
    assert_eq!(card_texts(&shown[0])[0], "ร้านป้าแดง");
    assert_eq!(s.flow(a).await, None);

    let mut approved = pending.into_active_model();
    approved.status = Set(shop::Status::Approved);
    approved.update(&s.db).await.unwrap();
    let shown = s.taps(a, "my_shop").await;
    assert_eq!(
        quick_replies(&shown[1]),
        ["ปิดร้าน", "เพิ่มสินค้า", "สินค้าของฉัน", "ตั้งค่าร้าน", "ดูหน้าร้าน"]
    );
    assert!(text(&shown[1]).contains("ตอนนี้ร้านเปิดอยู่"));
    assert_eq!(card_texts(&shown[0])[2..4], ["สถานะร้าน", "เปิดอยู่"]);
    assert_eq!(s.flow(a).await, None);

    // Another user's application, outrun by a shop made meanwhile
    let other = testing::new_user("U-other");
    let other = user::ActiveModel {
        zone: Set(Some("ซอย 1".to_owned())),
        house_number: Set(Some("1".to_owned())),
        ..other
    }
    .insert(&s.db)
    .await
    .unwrap()
    .id;
    s.fill(other).await;
    let made = s.new_shop(other, shop::Status::Pending).await;
    let refused = s.taps_latest(other, SUBMIT).await.0;
    assert_eq!(
        refused[1],
        s.templates.application_pending(&made.name, made.updated_at)
    );
    assert_eq!(s.flow(other).await, None);
    assert_eq!(s.shops().await.len(), 2);
    assert_eq!(s.pushes().await, []);
}

/// FR-UI-13: only the latest summary's buttons act; an older one's shows the application as it
/// is now, so it never sends what that summary didn't show.
#[tokio::test]
async fn only_the_latest_summary_acts() {
    let s = setup().await;
    let a = s.applicant;
    s.fill(a).await;
    let old = s.revision(a).await;

    let ask = s.corrects(a, Step::Name).await;
    assert!(text(&ask[0]).ends_with("(ตอนนี้: ร้านป้าแดง)"));
    assert_eq!(quick_replies(&ask[0]), ["ใช้ของเดิม", "ยกเลิก"]);
    // While the name is asked for, the summary's buttons ask for it again
    assert_eq!(
        s.taps(a, &format!("{SUBMIT}?r={old}")).await,
        s.taps(a, &format!("{EDIT}?r={old}")).await
    );
    let summary = s.types(a, "ร้านป้าแดง สาขาสอง").await;
    assert_eq!(card_texts(&summary[1])[0], "ร้านป้าแดง สาขาสอง");

    let stale = s.taps(a, &format!("{SUBMIT}?r={old}")).await;
    assert_eq!(stale, summary);
    assert_eq!(s.shops().await, []);
    s.taps_latest(a, SUBMIT).await;
    assert_eq!(s.shops().await[0].name, "ร้านป้าแดง สาขาสอง");

    // Keeping a part, and taking the fee step only for a shop newly delivering
    let other = testing::new_user("U-other");
    let other = user::ActiveModel {
        zone: Set(Some("ซอย 1".to_owned())),
        house_number: Set(Some("1".to_owned())),
        ..other
    }
    .insert(&s.db)
    .await
    .unwrap()
    .id;
    s.fill(other).await;
    s.corrects(other, Step::Fulfillment).await;
    assert_eq!(s.types(other, "ใช้ของเดิม").await.len(), 2);
    s.corrects(other, Step::Fulfillment).await;
    let fee = s.types(other, "ร้านไปส่ง").await;
    assert!(text(&fee[0]).ends_with("(ตอนนี้: ฿10)"), "{}", text(&fee[0]));
    let summary = s.types(other, "ใช้ของเดิม").await;
    let texts = card_texts(&summary[1]);
    assert!(
        texts.iter().any(|text| text == "ร้านไปส่ง ค่าส่ง ฿10"),
        "{texts:?}"
    );
}

/// The buttons of an application no longer in progress start nothing: whatever the user is
/// doing goes on undisturbed.
#[tokio::test]
async fn stale_buttons_start_nothing() {
    let s = setup().await;
    let a = s.applicant;
    // Correcting their address from "ข้อมูลของฉัน"
    s.handle(a, async |turn| {
        address::start(turn, &s.config, Ask::Address, Then::MyData).await
    })
    .await;
    let open = Item::MyShop.postback().button(labels::OPEN_SHOP.clone());
    for data in ["open_shop.submit?r=3", "open_shop.edit?r=3", AGAIN] {
        assert_eq!(
            s.taps(a, data).await,
            [s.templates.application_gone(open.clone())],
            "{data}"
        );
        assert_eq!(s.flow(a).await.as_deref(), Some(AddressFlow::NAME));
    }
}

/// FR-SHOP-04: once turned down, the owner sees why, and applies again from the summary of
/// what they sent, correcting what they need; the same shop then waits for an admin again.
#[tokio::test]
async fn a_rejected_applicant_applies_again() {
    let s = setup().await;
    let a = s.applicant;
    let rejected = shop::ActiveModel {
        rejection_reason: Set(Some("ข้อมูลไม่ครบ".to_owned())),
        promptpay_id: Set(Some("0812345678".to_owned())),
        ..s.new_shop(a, shop::Status::Rejected)
            .await
            .into_active_model()
    };
    let rejected = rejected.update(&s.db).await.unwrap();
    let photo = images::store(
        &s.db,
        Owner::Shop(rejected.id),
        images::prepare(PHOTO.to_vec(), &s.config.image)
            .await
            .unwrap(),
        &s.config.image,
        t0(),
    )
    .await
    .unwrap()
    .unwrap();

    let again = Postback::new(AGAIN).button(labels::APPLY_AGAIN.clone());
    assert_eq!(
        s.taps(a, "my_shop").await,
        [s.templates
            .application_rejected("ร้านป้าแดง", Some("ข้อมูลไม่ครบ"), again)]
    );
    let summary = s.taps(a, AGAIN).await;
    assert_eq!(summary.len(), 2);
    let base = &s.config.app.base_url;
    let url = images::product_image_url(base, photo).to_string();
    assert_eq!(hero(&summary[1]), Some(url.as_str()));
    assert!(card_texts(&summary[1]).contains(&"081-234-5678".to_owned()));

    s.corrects(a, Step::Description).await;
    s.types(a, "ไข่ไก่สด ส่งทุกเช้า").await;
    let (sent, _) = s.taps_latest(a, SUBMIT).await;
    assert_eq!(sent[0], s.templates.application_sent("ร้านป้าแดง"));
    let shops = s.shops().await;
    assert_eq!(shops.len(), 1);
    let shop = &shops[0];
    assert_eq!(
        (
            shop.id,
            shop.status,
            shop.description.as_str(),
            shop.rejection_reason.as_deref()
        ),
        (rejected.id, shop::Status::Pending, "ไข่ไก่สด ส่งทุกเช้า", None)
    );
    assert_eq!(shop.promptpay_id.as_deref(), Some("0812345678"));
    assert_eq!(
        (shop.created_at, shop.updated_at),
        (rejected.created_at, t0())
    );
    assert_eq!(s.images().await, [(OwnerType::Shop, shop.id, photo)]);
    assert_eq!(s.pushes().await.len(), 1);
}

/// NFR-SEC-10: a suspended user can't apply, and a suspension mid-flow ends the application.
#[tokio::test]
async fn a_suspended_user_cannot_apply() {
    let s = setup().await;
    let a = s.applicant;
    s.taps(a, "my_shop").await;
    let mut suspended = s.user(a).await.into_active_model();
    suspended.status = Set(user::Status::Suspended);
    suspended.update(&s.db).await.unwrap();

    let denied = vec![s.templates.denied(Denied::Suspended)];
    assert_eq!(s.types(a, "ร้านป้าแดง").await, denied);
    assert_eq!(s.flow(a).await, None);
    assert_eq!(s.taps(a, "my_shop").await, denied);
    assert_eq!(s.flow(a).await, None);
}
