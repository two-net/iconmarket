//! "ผู้ใช้" (FR-ADM-04): admins alone find users, by display name, phone or order number, and
//! suspend one or lift a suspension, theirs or their shop's, with a reason; the user is told,
//! a suspended one can't order or sell but can see their orders and get help, every refusal says
//! why, and buyers don't see their shop. Each change is audited, and applies once (FR-UI-13).
//! Each event is handled in a transaction committed once it is handled, with what it notifies
//! queued in it. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json, Uuid};
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder, TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::catalog::browse::listing;
use crate::config::{self, Config, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::{self, Templates};
use crate::orders::{self, cart};
use crate::persistence::entity::{audit_log, product, push_log, session};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;
use crate::users::help;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_ADMIN: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

const ORDER_NUMBER: &str = "IM-20261025-0007";

/// 26 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 26, 2, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    admin: user::Model,
    other_admin: user::Model,
    seller: user::Model,
    buyer: user::Model,
    shop: shop::Model,
    product: product::Model,
    order: order::Model,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    // Linked their menus already, so only the seller's is relinked when their roles change
    let admin = |line_user_id, name: &str| user::ActiveModel {
        display_name: Set(Some(name.to_owned())),
        is_admin: Set(true),
        menu_roles: Set(Some(user::MenuRoles::Admin)),
        ..testing::new_user(line_user_id)
    };
    let (admin, other_admin) = (
        admin(ADMIN, "แอดมินหนึ่ง").insert(&db).await.unwrap(),
        admin(OTHER_ADMIN, "แอดมินสอง").insert(&db).await.unwrap(),
    );
    let seller = user::ActiveModel {
        display_name: Set(Some("ป้าแดง".to_owned())),
        menu_roles: Set(Some(user::MenuRoles::Seller)),
        zone: Set(Some("ซอย 3".to_owned())),
        phone: Set(Some("0812345678".to_owned())),
        ..testing::new_user(SELLER)
    };
    let seller = seller.insert(&db).await.unwrap();
    let buyer = user::ActiveModel {
        display_name: Set(Some("สมชาย ใจดี".to_owned())),
        ..testing::new_user(BUYER)
    };
    let buyer = buyer.insert(&db).await.unwrap();
    // Deleted accounts are never found (FR-ONB-07)
    let deleted = user::ActiveModel {
        line_user_id: Set(None),
        display_name: Set(Some("สมชาย ลบแล้ว".to_owned())),
        status: Set(user::Status::Deleted),
        ..testing::new_user("U-deleted")
    };
    deleted.insert(&db).await.unwrap();
    let shop = shop::ActiveModel {
        phone: Set(Some("0898765432".to_owned())),
        ..testing::new_shop(seller.id)
    };
    let shop = shop.insert(&db).await.unwrap();
    let category = testing::new_category("ไข่/นม").insert(&db).await.unwrap();
    let product = testing::new_product(shop.id, category.id, "ไข่ไก่เบอร์ 2", Some(8))
        .insert(&db)
        .await
        .unwrap();
    let order = testing::new_order(buyer.id, shop.id, ORDER_NUMBER, Uuid::from_u128(7))
        .insert(&db)
        .await
        .unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        admin,
        other_admin,
        seller,
        buyer,
        shop,
        product,
        order,
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

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        let (messages, _) = self
            .handle(user_id, async |turn| tapped(turn, &postback).await)
            .await;
        messages
    }

    /// Admin `user_id` types `text` as who to find.
    async fn searches(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        let (messages, _) = self
            .handle(user_id, async |turn| {
                let session = turn.flow.as_ref().and_then(|flow| flow.read::<FindFlow>());
                let session = session.expect("finding a user");
                let answered = answer_query(turn, session, &input).await?;
                Ok(answered.expect("an answer"))
            })
            .await;
        messages
    }

    /// Admin `user_id` finds who `text` matches, from "ผู้ใช้".
    async fn finds(&self, user_id: i64, text: &str) -> Vec<Message> {
        self.taps(user_id, "users").await;
        self.searches(user_id, text).await
    }

    /// Admin `user_id` types `text` as the reason for the change they chose.
    async fn typed(&self, user_id: i64, text: &str) -> (Vec<Message>, Vec<i64>) {
        let input = Input::Text(text.to_owned());
        self.handle(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<ReasonFlow>());
            let session = session.expect("giving a reason");
            let answered = answer_reason(turn, session, &input).await?;
            Ok(answered.expect("an answer"))
        })
        .await
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        self.typed(user_id, text).await.0
    }

    /// Admin `user_id` taps `action` on the card of user `target`, then gives `reason`.
    async fn changes(&self, user_id: i64, action: &str, target: i64, reason: &str) -> Vec<Message> {
        self.taps(user_id, &format!("{action}?u={target}")).await;
        self.types(user_id, reason).await
    }

    async fn user(&self, id: i64) -> user::Model {
        user::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn the_shop(&self) -> shop::Model {
        shop::Entity::find_by_id(self.shop.id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// The reply of user `user_id`'s turn, given a turn: `reply`.
    async fn at(
        &self,
        user_id: i64,
        reply: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        self.handle(user_id, reply).await.0
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

/// The labels of the Quick Replies of `message`.
fn quick_replies(message: &Message) -> Vec<&str> {
    message
        .quick_reply
        .as_ref()
        .unwrap()
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
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

/// NFR-SEC-02: only admins find users or change them.
#[tokio::test]
async fn only_admins_find_or_change_users() {
    let s = setup().await;
    let suspend = format!("{SUSPEND}?u={}", s.buyer.id);
    for data in ["users", suspend.as_str()] {
        assert_eq!(
            s.taps(s.seller.id, data).await,
            [s.templates.denied(Denied::NotAllowed)],
            "{data}"
        );
    }
    assert!(!s.in_flow(s.seller.id).await);
    assert_eq!(s.user(s.buyer.id).await.status, user::Status::Active);
    assert_eq!((s.pushes().await, s.audited().await), (vec![], vec![]));
}

/// FR-ADM-04: users are found by display name, whatever its case, by phone, as typed, of theirs
/// or their shop's, or by order number, its buyer and its shop's owner; never a deleted one. Each
/// has a card, with the buttons of the changes that apply.
#[tokio::test]
async fn users_are_found_by_name_phone_or_order_number() {
    let s = setup().await;
    let asked = s.taps(s.admin.id, "users").await;
    assert_eq!(
        asked,
        [templates::with_cancel(s.templates.ask_user_query())]
    );
    assert!(s.in_flow(s.admin.id).await);

    let found = s.searches(s.admin.id, " สมชาย ").await;
    let suspend = |user: &user::Model| format!("{SUSPEND}?u={}", user.id);
    let card = UserCard {
        display_name: Some("สมชาย ใจดี"),
        seller: false,
        admin: false,
        suspended: false,
        suspension_reason: None,
        zone: None,
        joined: testing::at(),
        shop: None,
        buttons: vec![
            Postback::parse(&suspend(&s.buyer))
                .unwrap()
                .button(labels::SUSPEND_ACCOUNT.clone()),
        ],
    };
    assert_eq!(found, s.templates.users_found(&[card], false));
    assert!(!s.in_flow(s.admin.id).await);
    assert_eq!(
        quick_replies(&found[0]),
        [labels::FIND_AGAIN.as_str(), "เมนู"]
    );

    let mut latin = s.buyer.clone().into_active_model();
    latin.display_name = Set(Some("Somchai J.".to_owned()));
    latin.update(&s.db).await.unwrap();
    let found = s.finds(s.admin.id, "SOMCHAI").await;
    assert_eq!(card_buttons(&found[0]), [[suspend(&s.buyer)]]);

    // Their phone, or their shop's, however written; the seller's card has their shop's
    for phone in ["081-234-5678", "+66 89 876 5432"] {
        let found = s.finds(s.admin.id, phone).await;
        assert_eq!(card_buttons(&found[0]), [[suspend(&s.seller)]], "{phone}");
    }
    // The order's buyer, then its shop's owner
    let found = s.finds(s.admin.id, "im-20261025-0007").await;
    assert_eq!(
        card_buttons(&found[0]),
        [[suspend(&s.buyer)], [suspend(&s.seller)]]
    );
    // An admin can't be suspended
    let found = s.finds(s.admin.id, "แอดมิน").await;
    assert_eq!(card_buttons(&found[0]), [Vec::<String>::new(), Vec::new()]);

    // Nothing found: the step asks again
    s.taps(s.admin.id, "users").await;
    let none = s.searches(s.admin.id, "ลบแล้ว").await;
    assert_eq!(
        none,
        [templates::with_cancel(s.templates.no_user_found("ลบแล้ว"))]
    );
    assert!(s.in_flow(s.admin.id).await);

    // More than a search shows
    for i in 0..=templates::USERS_FOUND {
        let user = user::ActiveModel {
            display_name: Set(Some(format!("ลูกค้า {i:02}"))),
            ..testing::new_user(&format!("U-customer-{i}"))
        };
        user.insert(&s.db).await.unwrap();
    }
    let many = s.searches(s.admin.id, "ลูกค้า").await;
    assert_eq!(many.len(), 2);
    assert!(text(&many[0]).starts_with(&format!("พบมากกว่า {} คนค่ะ", templates::USERS_FOUND)));
    assert_eq!(card_buttons(&many[1]).len(), templates::USERS_FOUND);
}

/// Acceptance: a suspended user is told why; their attempt to order is refused with the reason,
/// while their orders and help still open (FR-ADM-04, NFR-SEC-10). The suspension is audited.
#[tokio::test]
async fn a_suspended_user_is_told_why_and_cannot_order() {
    let s = setup().await;
    let asked = s
        .taps(s.admin.id, &format!("{SUSPEND}?u={}", s.buyer.id))
        .await;
    assert!(
        text(&asked[0]).starts_with("ระงับบัญชีของ \"สมชาย ใจดี\" เพราะอะไรคะ"),
        "{}",
        text(&asked[0])
    );
    assert_eq!(
        quick_replies(&asked[0]),
        ["หลอกลวง/โกง", "สินค้าต้องห้าม", "ไม่สุภาพ", "ยกเลิก"]
    );
    assert_eq!(s.user(s.buyer.id).await.status, user::Status::Active);

    let reply = s.types(s.admin.id, "โกงเงินมัดจำ").await;
    assert_eq!(reply, [s.templates.user_suspended(Some("สมชาย ใจดี"))]);
    assert!(!s.in_flow(s.admin.id).await);
    let buyer = s.user(s.buyer.id).await;
    assert_eq!(
        (
            buyer.status,
            buyer.suspension_reason.as_deref(),
            buyer.updated_at
        ),
        (user::Status::Suspended, Some("โกงเงินมัดจำ"), t0())
    );
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin.id),
            "user.suspended".to_owned(),
            "user".to_owned(),
            Some(s.buyer.id),
            json!({ "reason": "โกงเงินมัดจำ" })
        )]
    );
    let suspended = s.templates.account_suspended_notice("โกงเงินมัดจำ");
    assert_eq!(
        s.pushes().await,
        [(
            "user.suspended".to_owned(),
            BUYER.to_owned(),
            queued(&[suspended])
        )]
    );

    // Ordering is refused, with the reason
    let add = format!(
        "{}?{}={}",
        orders::ADD_TO_CART,
        orders::PRODUCT_ID,
        s.product.id
    );
    let add = Postback::parse(&add).unwrap();
    let refused = s
        .at(s.buyer.id, async |turn| cart::tapped(turn, &add).await)
        .await;
    assert_eq!(
        refused,
        [s.templates.refused(Denied::Suspended, Some("โกงเงินมัดจำ"))]
    );
    assert!(
        text(&refused[0]).contains("บัญชีของคุณถูกระงับการใช้งานชั่วคราว เหตุผล: โกงเงินมัดจำ"),
        "{}",
        text(&refused[0])
    );
    // Their orders and help still open
    let actor = Actor::load(&s.db, s.buyer.id).await.unwrap().unwrap();
    let view = access::Action::ViewOrder(s.order.id);
    assert_eq!(authorize(&s.db, &actor, view).await.unwrap(), Ok(()));
    let help = s.at(s.buyer.id, async |turn| help::show(turn).await).await;
    assert_ne!(help[0], s.templates.denied(Denied::Suspended));
    assert_eq!(
        text(&s.taps(s.buyer.id, "users").await[0]),
        text(&refused[0])
    );
}

/// FR-ADM-04: buyers don't see a suspended user's shop until the suspension is lifted, which
/// asks why and tells the user; a suspended shop is restored from its owner's card, which makes
/// them a seller again.
#[tokio::test]
async fn lifting_a_suspension_restores_the_account_or_its_shop() {
    let s = setup().await;
    s.changes(s.admin.id, SUSPEND, s.seller.id, "ขายของปลอม")
        .await;
    assert!(listing::shop(&s.db, s.shop.id).await.unwrap().is_none());
    assert!(
        listing::product(&s.db, s.product.id)
            .await
            .unwrap()
            .is_none()
    );

    let asked = s
        .taps(s.admin.id, &format!("{UNSUSPEND}?u={}", s.seller.id))
        .await;
    assert!(
        text(&asked[0]).starts_with("ยกเลิกการระงับบัญชีของ \"ป้าแดง\" เพราะอะไรคะ"),
        "{}",
        text(&asked[0])
    );
    assert_eq!(quick_replies(&asked[0]), ["แก้ไขแล้ว", "ตรวจสอบแล้ว", "ยกเลิก"]);
    let reply = s.types(s.admin.id, "ตรวจสอบแล้ว").await;
    assert_eq!(reply, [s.templates.user_unsuspended(Some("ป้าแดง"))]);
    let seller = s.user(s.seller.id).await;
    assert_eq!(
        (seller.status, seller.suspension_reason),
        (user::Status::Active, None)
    );
    assert!(listing::shop(&s.db, s.shop.id).await.unwrap().is_some());
    assert!(
        listing::product(&s.db, s.product.id)
            .await
            .unwrap()
            .is_some()
    );

    // The shop suspended from the review queue
    let mut suspended = s.shop.clone().into_active_model();
    suspended.status = Set(shop::Status::Suspended);
    suspended.suspension_reason = Set(Some("ขายของปลอม".to_owned()));
    suspended.update(&s.db).await.unwrap();
    let found = s.finds(s.admin.id, "ป้าแดง").await;
    assert_eq!(
        card_buttons(&found[0]),
        [[
            format!("{SUSPEND}?u={}", s.seller.id),
            format!("{UNSUSPEND_SHOP}?u={}", s.seller.id),
        ]]
    );
    let asked = s
        .taps(s.admin.id, &format!("{UNSUSPEND_SHOP}?u={}", s.seller.id))
        .await;
    assert!(
        text(&asked[0]).starts_with("ยกเลิกการระงับร้านป้าแดงเพราะอะไรคะ"),
        "{}",
        text(&asked[0])
    );
    let (reply, relink) = s.typed(s.admin.id, "แก้ไขแล้ว").await;
    assert_eq!(reply, [s.templates.shop_unsuspended("ร้านป้าแดง")]);
    assert_eq!(relink, [s.seller.id]);
    let shop = s.the_shop().await;
    assert_eq!(
        (shop.status, shop.suspension_reason, shop.updated_at),
        (shop::Status::Approved, None, t0())
    );
    let actor = Actor::load(&s.db, s.seller.id).await.unwrap().unwrap();
    assert!(actor.roles().seller);

    let seller = Some(s.seller.id);
    assert_eq!(
        s.audited().await,
        [
            (
                Some(s.admin.id),
                "user.suspended".to_owned(),
                "user".to_owned(),
                seller,
                json!({ "reason": "ขายของปลอม" })
            ),
            (
                Some(s.admin.id),
                "user.unsuspended".to_owned(),
                "user".to_owned(),
                seller,
                json!({ "reason": "ตรวจสอบแล้ว" })
            ),
            (
                Some(s.admin.id),
                "shop.unsuspended".to_owned(),
                "shop".to_owned(),
                Some(s.shop.id),
                json!({ "name": "ร้านป้าแดง", "reason": "แก้ไขแล้ว" })
            ),
        ]
    );
    let my_shop = Item::MyShop.postback().button(labels::MY_SHOP.clone());
    assert_eq!(
        s.pushes().await,
        [
            (
                "user.suspended".to_owned(),
                SELLER.to_owned(),
                queued(&[s.templates.account_suspended_notice("ขายของปลอม")])
            ),
            (
                "user.unsuspended".to_owned(),
                SELLER.to_owned(),
                queued(&[s.templates.account_unsuspended_notice()])
            ),
            (
                "shop.unsuspended".to_owned(),
                SELLER.to_owned(),
                queued(&[s.templates.shop_unsuspended_notice("ร้านป้าแดง", my_shop)])
            ),
        ]
    );
}

/// FR-ADM-01, FR-UI-13: an admin can't be suspended, themselves included; a change already made,
/// by an earlier tap or another admin, isn't made again, and shows the user as they are now; a
/// change whose admin lost the role mid-flow isn't made.
#[tokio::test]
async fn changes_apply_once_and_never_to_admins() {
    let s = setup().await;
    for admin in [&s.other_admin, &s.admin] {
        let refused = s
            .taps(s.admin.id, &format!("{SUSPEND}?u={}", admin.id))
            .await;
        assert_eq!(refused[0], s.templates.admin_not_suspended());
        assert_eq!(card_buttons(&refused[1]), [Vec::<String>::new()]);
        assert!(!s.in_flow(s.admin.id).await);
    }

    // Suspended by another admin while the reason was typed
    let suspend = format!("{SUSPEND}?u={}", s.buyer.id);
    s.taps(s.admin.id, &suspend).await;
    s.changes(s.other_admin.id, SUSPEND, s.buyer.id, "หลอกลวง/โกง")
        .await;
    let already = s.types(s.admin.id, "ไม่สุภาพ").await;
    assert_eq!(
        already[0],
        s.templates.account_already(Some("สมชาย ใจดี"), true)
    );
    assert_eq!(
        card_buttons(&already[1]),
        [[format!("{UNSUSPEND}?u={}", s.buyer.id)]]
    );
    assert!(!s.in_flow(s.admin.id).await);
    assert_eq!(
        s.user(s.buyer.id).await.suspension_reason.as_deref(),
        Some("หลอกลวง/โกง")
    );
    assert_eq!(s.taps(s.admin.id, &suspend).await, already);

    // Nothing to lift
    let unsuspend = format!("{UNSUSPEND}?u={}", s.seller.id);
    let active = s.taps(s.admin.id, &unsuspend).await;
    assert_eq!(active[0], s.templates.account_already(Some("ป้าแดง"), false));
    let unsuspend_shop = format!("{UNSUSPEND_SHOP}?u={}", s.seller.id);
    assert_eq!(
        s.taps(s.admin.id, &unsuspend_shop).await[0],
        s.templates.shop_not_suspended()
    );
    assert_eq!(s.audited().await.len(), 1);

    // Gone, or never there
    for data in [
        format!("{SUSPEND}?u={}", s.buyer.id + 100),
        format!("{SUSPEND}?x=1"),
        "accounts.delete?u=1".to_owned(),
    ] {
        assert_eq!(
            s.taps(s.admin.id, &data).await,
            [s.templates.denied(Denied::NotFound)],
            "{data}"
        );
    }

    // The admin role lost mid-flow
    s.taps(s.admin.id, &format!("{SUSPEND}?u={}", s.seller.id))
        .await;
    let mut demoted = s.admin.clone().into_active_model();
    demoted.is_admin = Set(false);
    demoted.update(&s.db).await.unwrap();
    assert_eq!(
        s.types(s.admin.id, "ขายของปลอม").await,
        [s.templates.denied(Denied::NotAllowed)]
    );
    assert!(!s.in_flow(s.admin.id).await);
    assert_eq!(s.user(s.seller.id).await.status, user::Status::Active);
}
