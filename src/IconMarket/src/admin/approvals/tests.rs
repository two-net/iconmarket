//! "อนุมัติร้าน" (FR-ADM-02): admins alone see the applications, oldest first, and approve or
//! turn one down with a reason; the owner is told, an approved one gets the seller's menu, and
//! each decision is audited. A card that no longer matches the application, by a decision or
//! an application made since, shows it as it is instead of acting (FR-UI-13). Each event is
//! handled in a transaction committed once it is handled, with what it notifies queued in it.
//! The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder, TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::{self, PromptPay, ShopCard};
use crate::persistence::entity::{audit_log, push_log, session};
use crate::persistence::{orm, testing};
use crate::thai::Money;
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_ADMIN: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const OWNER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

#[test]
fn reasons_are_tidied_and_bounded() {
    assert_eq!(
        reason("  ไม่ได้อยู่ \n ในหมู่บ้าน ").as_deref(),
        Ok("ไม่ได้อยู่ ในหมู่บ้าน")
    );
    let longest = "ก".repeat(REASON_CHARS);
    assert_eq!(reason(&longest), Ok(longest.clone()));
    assert_eq!(
        reason(&format!("{longest}ก")),
        Err(Refused::TooLong(REASON_CHARS))
    );
    assert_eq!(reason(" \n "), Err(Refused::Empty));
}

/// FR-LINE-11: a full page of the largest applications, with every button, fits one message.
#[test]
fn a_full_page_of_applications_fits() {
    let config = config::sample();
    let t = Templates::new(&config.app);
    let (name, description, address) = (
        "ข".repeat(shops::NAME_CHARS),
        "ข".repeat(shops::DESCRIPTION_CHARS),
        "ข".repeat(shops::PICKUP_ADDRESS_CHARS),
    );
    let photo = images::product_image_url(&config.app.base_url, Uuid::from_u128(u128::MAX));
    let display_name = "ข".repeat(100);
    let action = |label: &Label| {
        let data = format!("approvals.approve?s={}&v={}", i64::MAX, i64::MAX);
        Postback::parse(&data).unwrap().button(label.clone())
    };
    let card = || ShopCard {
        name: &name,
        description: &description,
        photo: Some(&photo),
        pickup_address: Some(&address),
        pickup_location: true,
        phone: Some("0812345678"),
        offers_pickup: true,
        offers_delivery: true,
        delivery_fee: shops::MAX_DELIVERY_FEE,
        minimum_order: Some(Money::from_satang(9_999_999)),
        promptpay: PromptPay::Given(true),
        running: None,
        applicant: Some(Applicant {
            display_name: Some(&display_name),
            at: t0(),
        }),
        buttons: vec![action(&labels::APPROVE), action(&labels::REJECT)],
    };
    let cards: Vec<_> = (0..=templates::APPLICATIONS_PER_PAGE)
        .map(|_| card())
        .collect();
    let page = t.applications(&cards, 0, |next| format!("approve_shops?page={next}"));
    limits::check(std::slice::from_ref(&page)).unwrap();
    let Body::Flex {
        contents: FlexContainer::Carousel(carousel),
        ..
    } = &page.body
    else {
        panic!("not a carousel");
    };
    assert_eq!(
        carousel.contents.len(),
        templates::APPLICATIONS_PER_PAGE + 1
    );
    limits::check(&[t.shop_application_alert(&card())]).unwrap();
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    admin: i64,
    other_admin: i64,
    owner: user::Model,
    /// The owner's application, awaiting an admin
    shop: shop::Model,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    // Linked the admin's menu already, so only the owner's is relinked
    let admin = |line_user_id| user::ActiveModel {
        is_admin: Set(true),
        menu_roles: Set(Some(user::MenuRoles::Admin)),
        ..testing::new_user(line_user_id)
    };
    let (admin, other_admin) = (
        admin(ADMIN).insert(&db).await.unwrap().id,
        admin(OTHER_ADMIN).insert(&db).await.unwrap().id,
    );
    let owner = user::ActiveModel {
        display_name: Set(Some("ป้าแดง".to_owned())),
        ..testing::new_user(OWNER)
    };
    let owner = owner.insert(&db).await.unwrap();
    let shop = shop::ActiveModel {
        status: Set(shop::Status::Pending),
        phone: Set(Some("0812345678".to_owned())),
        pickup_address: Set(Some("ซอย 3 บ้านเลขที่ 45/2".to_owned())),
        ..testing::new_shop(owner.id)
    };
    let shop = shop.insert(&db).await.unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        admin,
        other_admin,
        owner,
        shop,
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
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.config.notification, t0())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        limits::check(&messages).unwrap();
        (messages, relink)
    }

    async fn tapped(&self, user_id: i64, data: &str) -> (Vec<Message>, Vec<i64>) {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            tapped(turn, &self.config, &postback).await
        })
        .await
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.tapped(user_id, data).await.0
    }

    /// User `user_id` types `text` as the reason they turn an application down.
    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        let (messages, _) = self
            .handle(user_id, async |turn| {
                let session = turn
                    .flow
                    .as_ref()
                    .and_then(|flow| flow.read::<RejectionFlow>());
                let session = session.expect("turning an application down");
                let answered = answer(turn, &self.config, session, &input).await?;
                Ok(answered.expect("an answer"))
            })
            .await;
        messages
    }

    async fn the_shop(&self) -> shop::Model {
        shop::Entity::find_by_id(self.shop.id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// The postback of `action` on the card of the shop's application as it is now.
    async fn button(&self, action: &str) -> String {
        let shop = self.the_shop().await;
        format!("{action}?s={}&v={}", shop.id, version(&shop))
    }

    async fn in_flow(&self, user_id: i64) -> bool {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .is_some()
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

/// The postback data of the buttons of each card of `message`, a carousel or one card.
fn card_buttons(message: &Message) -> Vec<Vec<String>> {
    let bubbles: Vec<&crate::line::flex::Bubble> = match &message.body {
        Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } => carousel.contents.iter().collect(),
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => vec![&**bubble],
        body => panic!("not a card: {body:?}"),
    };
    bubbles
        .into_iter()
        .map(|bubble| {
            let Some(footer) = &bubble.footer else {
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
        })
        .collect()
}

/// NFR-SEC-02: only admins see or decide applications.
#[tokio::test]
async fn only_admins_decide() {
    let s = setup().await;
    let owner = s.owner.id;
    let approve = s.button(APPROVE).await;
    for data in ["approve_shops", approve.as_str()] {
        assert_eq!(
            s.taps(owner, data).await,
            [s.templates.denied(Denied::NotAllowed)],
            "{data}"
        );
    }
    assert_eq!(s.the_shop().await.status, shop::Status::Pending);
    assert_eq!((s.pushes().await, s.audited().await), (vec![], vec![]));
}

/// FR-ADM-02: the applications awaiting an admin, oldest first, a card each with who applied
/// and its buttons, paged; decided ones aren't listed.
#[tokio::test]
async fn the_applications_are_listed_oldest_first() {
    let s = setup().await;
    let mut decided = s.shop.clone().into_active_model();
    decided.status = Set(shop::Status::Rejected);
    decided.update(&s.db).await.unwrap();
    let none = s.taps(s.admin, "approve_shops").await;
    assert!(text(&none[0]).starts_with("ตอนนี้ไม่มีคำขอเปิดร้านที่รออนุมัติ"));
    let mut pending = s.shop.clone().into_active_model();
    pending.status = Set(shop::Status::Pending);
    pending.update(&s.db).await.unwrap();

    let mut later = Vec::new();
    for i in 1..=templates::APPLICATIONS_PER_PAGE {
        let user = testing::new_user(&format!("U-applicant-{i}"));
        let user = user.insert(&s.db).await.unwrap();
        let shop = shop::ActiveModel {
            status: Set(shop::Status::Pending),
            name: Set(format!("ร้านที่ {i}")),
            updated_at: Set(testing::at() + TimeDelta::minutes(i64::try_from(i).unwrap())),
            ..testing::new_shop(user.id)
        };
        later.push(shop.insert(&s.db).await.unwrap());
    }
    let approved = testing::new_user("U-approved").insert(&s.db).await.unwrap();
    testing::new_shop(approved.id).insert(&s.db).await.unwrap();

    let first = s.taps(s.admin, "approve_shops").await;
    let cards = card_buttons(&first[0]);
    assert_eq!(cards.len(), templates::APPLICATIONS_PER_PAGE + 1);
    let shown = |shop: &shop::Model| {
        vec![
            format!("approvals.approve?s={}&v={}", shop.id, version(shop)),
            format!("approvals.reject?s={}&v={}", shop.id, version(shop)),
        ]
    };
    assert_eq!(cards[0], shown(&s.shop));
    assert_eq!(cards[1], shown(&later[0]));
    let more = cards.last().unwrap();
    assert!(more.is_empty(), "the \"ดูเพิ่มเติม\" bubble has no footer");
    let quick_reply = first[0].quick_reply.as_ref().unwrap();
    assert_eq!(quick_reply.items.len(), 2);

    let second = s.taps(s.admin, "approve_shops?page=1").await;
    assert_eq!(card_buttons(&second[0]), [shown(later.last().unwrap())]);
    // A page past the last starts over
    assert_eq!(s.taps(s.admin, "approve_shops?page=9").await, first);
}

/// Acceptance (Appendix A.5): approving makes the owner a seller, tells them so with a button
/// to start listing, links the seller's menu, and is audited.
#[tokio::test]
async fn approving_makes_a_seller_and_tells_the_owner() {
    let s = setup().await;
    let approve = s.button(APPROVE).await;
    let (reply, relink) = s.tapped(s.admin, &approve).await;
    assert_eq!(reply, [s.templates.shop_approved("ร้านป้าแดง")]);
    assert_eq!(text(&reply[0]), "อนุมัติร้านป้าแดงแล้วค่ะ");
    assert_eq!(relink, [s.owner.id]);

    let shop = s.the_shop().await;
    assert_eq!(
        (shop.status, shop.rejection_reason, shop.updated_at),
        (shop::Status::Approved, None, t0())
    );
    let actor = Actor::load(&s.db, s.owner.id).await.unwrap().unwrap();
    assert!(actor.roles().seller);
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin),
            "shop.approved".to_owned(),
            "shop".to_owned(),
            Some(s.shop.id),
            json!({ "name": "ร้านป้าแดง" })
        )]
    );
    let add_product = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    let approved = s.templates.shop_approved_notice("ร้านป้าแดง", add_product);
    assert_eq!(
        s.pushes().await,
        [(
            "shop.approved".to_owned(),
            OWNER.to_owned(),
            queued(&[approved])
        )]
    );
}

/// FR-ADM-02, FR-SHOP-04: turning an application down asks why, keeps the reason, tells the
/// owner with a button to apply again, and is audited with the reason.
#[tokio::test]
async fn rejecting_asks_why_and_tells_the_owner() {
    let s = setup().await;
    let reject = s.button(REJECT).await;
    let asked = s.taps(s.admin, &reject).await;
    assert!(text(&asked[0]).starts_with("ไม่อนุมัติร้านป้าแดงเพราะอะไรคะ"));
    let reasons: Vec<_> = asked[0]
        .quick_reply
        .as_ref()
        .unwrap()
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "ข้อมูลไม่ครบ",
            "ไม่ได้อยู่ในหมู่บ้าน",
            "สินค้าต้องห้าม",
            "ติดต่อไม่ได้",
            "ยกเลิก"
        ]
    );
    assert!(s.in_flow(s.admin).await);
    // Nothing is decided until the reason is given
    assert_eq!(s.the_shop().await.status, shop::Status::Pending);
    let too_long = "ก".repeat(REASON_CHARS + 1);
    assert_eq!(
        s.types(s.admin, &too_long).await,
        [templates::with_cancel(
            s.templates.text_too_long(REASON_CHARS)
        )]
    );

    let reply = s.types(s.admin, "ข้อมูลไม่ครบ").await;
    assert_eq!(reply, [s.templates.shop_rejected("ร้านป้าแดง")]);
    assert!(!s.in_flow(s.admin).await);
    let shop = s.the_shop().await;
    assert_eq!(
        (shop.status, shop.rejection_reason.as_deref()),
        (shop::Status::Rejected, Some("ข้อมูลไม่ครบ"))
    );
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin),
            "shop.rejected".to_owned(),
            "shop".to_owned(),
            Some(s.shop.id),
            json!({ "name": "ร้านป้าแดง", "reason": "ข้อมูลไม่ครบ" })
        )]
    );
    let again = Postback::new(shops::AGAIN).button(labels::APPLY_AGAIN.clone());
    let rejected = s
        .templates
        .shop_rejected_notice("ร้านป้าแดง", "ข้อมูลไม่ครบ", again);
    assert_eq!(
        s.pushes().await,
        [(
            "shop.rejected".to_owned(),
            OWNER.to_owned(),
            queued(&[rejected])
        )]
    );
}

/// FR-UI-13: a second tap, or another admin's on the same card, changes nothing more and says
/// how it was decided; a card sent before the owner applied again shows the application as it
/// is now, with buttons for it.
#[tokio::test]
async fn a_card_that_no_longer_matches_shows_the_application_as_it_is() {
    let s = setup().await;
    let (approve, reject) = (s.button(APPROVE).await, s.button(REJECT).await);
    s.taps(s.admin, &approve).await;
    let decided = vec![s.templates.application_decided("ร้านป้าแดง", true)];
    assert_eq!(s.taps(s.admin, &approve).await, decided);
    assert_eq!(s.taps(s.other_admin, &reject).await, decided);
    assert!(!s.in_flow(s.other_admin).await);
    assert_eq!((s.pushes().await.len(), s.audited().await.len()), (1, 1));

    // Applied for again since the card was sent
    let mut again = s.the_shop().await.into_active_model();
    again.status = Set(shop::Status::Pending);
    again.updated_at = Set(t0() + TimeDelta::hours(1));
    let again = again.update(&s.db).await.unwrap();
    let (changed, relink) = s.tapped(s.admin, &approve).await;
    assert_eq!(changed[0], s.templates.application_changed());
    assert_eq!(
        card_buttons(&changed[1]),
        [vec![
            format!("approvals.approve?s={}&v={}", again.id, version(&again)),
            format!("approvals.reject?s={}&v={}", again.id, version(&again)),
        ]]
    );
    assert!(relink.is_empty(), "{relink:?}");
    assert_eq!(s.the_shop().await.status, shop::Status::Pending);

    // Gone for good
    let mut closed = s.the_shop().await.into_active_model();
    closed.status = Set(shop::Status::Closed);
    closed.update(&s.db).await.unwrap();
    let gone = s.taps(s.admin, &s.button(APPROVE).await).await;
    assert_eq!(gone[0], s.templates.denied(Denied::NotFound));
    let missing = format!("{APPROVE}?s={}&v=1", s.shop.id + 100);
    assert_eq!(s.taps(s.admin, &missing).await[0], gone[0]);
    assert_eq!(
        s.taps(s.admin, "approvals.approve?s=1").await,
        [s.templates.denied(Denied::NotFound)]
    );
}

/// The reason is typed after the decision is chosen: another admin may decide meanwhile, or
/// the admin lose their role, and then nothing is turned down.
#[tokio::test]
async fn nothing_is_turned_down_once_it_no_longer_can_be() {
    let s = setup().await;
    let reject = s.button(REJECT).await;
    s.taps(s.admin, &reject).await;
    s.taps(s.other_admin, &s.button(APPROVE).await).await;
    assert_eq!(
        s.types(s.admin, "ข้อมูลไม่ครบ").await,
        [s.templates.application_decided("ร้านป้าแดง", true)]
    );
    assert!(!s.in_flow(s.admin).await);
    assert_eq!(s.the_shop().await.status, shop::Status::Approved);

    let mut pending = s.the_shop().await.into_active_model();
    pending.status = Set(shop::Status::Pending);
    pending.update(&s.db).await.unwrap();
    s.taps(s.admin, &s.button(REJECT).await).await;
    let mut demoted = user::Entity::find_by_id(s.admin)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    demoted.is_admin = Set(false);
    demoted.update(&s.db).await.unwrap();
    assert_eq!(
        s.types(s.admin, "ข้อมูลไม่ครบ").await,
        [s.templates.denied(Denied::NotAllowed)]
    );
    assert!(!s.in_flow(s.admin).await);
    assert_eq!(s.the_shop().await.status, shop::Status::Pending);
    let rejected = s
        .audited()
        .await
        .into_iter()
        .filter(|(_, action, ..)| action == "shop.rejected")
        .count();
    assert_eq!(rejected, 0);
}
