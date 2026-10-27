//! "รายการรอตรวจ" (FR-ADM-03): admins alone see what moderation held and what users reported, a
//! card for each product or shop, and approve it, remove the product, suspend the shop or dismiss
//! the reports; the owner is told of a removal or a suspension and why, buyers see what is
//! decided, and each decision is audited. A card that no longer matches, by a decision or an edit
//! made since, shows it as it is instead of acting (FR-UI-13). Each event is handled in a
//! transaction committed once it is handled, with what it notifies queued in it. The `async`
//! tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder, TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::catalog::browse::listing;
use crate::catalog::{products, shops};
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates;
use crate::persistence::entity::{audit_log, push_log, session};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_ADMIN: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

/// What moderation filed for the held product
const HELD_FOR: &str = "มีคำต้องห้าม \"กัญชา\"";

/// 26 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 26, 2, 0, 0).unwrap()
}

/// FR-LINE-11: a full page of the largest cards, with every button, fits one message.
#[test]
fn a_full_page_of_the_queue_fits() {
    let config = config::sample();
    let t = Templates::new(&config.app);
    let (name, shop, reason, reporter) = (
        "ข".repeat(products::NAME_CHARS),
        "ข".repeat(shops::NAME_CHARS),
        "ข".repeat(REASON_CHARS),
        "ข".repeat(100),
    );
    let photo = images::product_image_url(&config.app.base_url, Uuid::from_u128(u128::MAX));
    let action = |action: &str, label: &Label| {
        let data = format!("{action}?p={}&v={}&r={}", i64::MAX, i64::MAX, i64::MAX);
        Postback::parse(&data).unwrap().button(label.clone())
    };
    let card = || ReviewCard {
        what: Reported::Product {
            name: &name,
            shop: &shop,
        },
        photo: Some(&photo),
        state: Reviewed::Product {
            status: product::Status::Hidden,
            shop: shop::Status::Held,
        },
        reports: (0..=templates::REVIEW_REPORTS)
            .map(|_| ReviewReport {
                by: Reporter::User(Some(&reporter)),
                reason: &reason,
                at: t0(),
            })
            .collect(),
        buttons: vec![
            action(REMOVE, &labels::REMOVE_PRODUCT),
            action(SUSPEND, &labels::SUSPEND_SHOP),
            action(DISMISS, &labels::DISMISS),
        ],
    };
    let cards: Vec<_> = (0..=templates::REVIEWS_PER_PAGE).map(|_| card()).collect();
    let page = t.review_queue(&cards, 0, |next| format!("review_queue?page={next}"));
    limits::check(std::slice::from_ref(&page)).unwrap();
    let Body::Flex {
        contents: FlexContainer::Carousel(carousel),
        ..
    } = &page.body
    else {
        panic!("not a carousel");
    };
    assert_eq!(carousel.contents.len(), templates::REVIEWS_PER_PAGE + 1);
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    admin: i64,
    other_admin: i64,
    seller: user::Model,
    buyer: user::Model,
    shop: shop::Model,
    /// Held for review by moderation, which filed a report of no one's
    held: product::Model,
    /// Listed, and reported by the buyer
    listed: product::Model,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    // Linked their menus already, so only the seller's is relinked when their roles change
    let admin = |line_user_id| user::ActiveModel {
        is_admin: Set(true),
        menu_roles: Set(Some(user::MenuRoles::Admin)),
        ..testing::new_user(line_user_id)
    };
    let (admin, other_admin) = (
        admin(ADMIN).insert(&db).await.unwrap().id,
        admin(OTHER_ADMIN).insert(&db).await.unwrap().id,
    );
    let seller = user::ActiveModel {
        display_name: Set(Some("ป้าแดง".to_owned())),
        menu_roles: Set(Some(user::MenuRoles::Seller)),
        ..testing::new_user(SELLER)
    };
    let seller = seller.insert(&db).await.unwrap();
    let buyer = user::ActiveModel {
        display_name: Set(Some("สมชาย".to_owned())),
        ..testing::new_user(BUYER)
    };
    let buyer = buyer.insert(&db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&db).await.unwrap();
    let category = testing::new_category("ไข่/นม").insert(&db).await.unwrap();
    let held = product::ActiveModel {
        status: Set(product::Status::Held),
        ..testing::new_product(shop.id, category.id, "ไข่ไก่เบอร์ 2", Some(8))
    };
    let held = held.insert(&db).await.unwrap();
    let listed = testing::new_product(shop.id, category.id, "ขนมครก", None)
        .insert(&db)
        .await
        .unwrap();
    let config = config::sample();
    let s = Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        admin,
        other_admin,
        seller,
        buyer,
        shop,
        held,
        listed,
    };
    s.report(
        None,
        Target::Product(s.held.id),
        HELD_FOR,
        TimeDelta::hours(2),
    )
    .await;
    let buyer = Some(s.buyer.id);
    s.report(
        buyer,
        Target::Product(s.listed.id),
        "ข้อมูลไม่ตรง",
        TimeDelta::hours(1),
    )
    .await;
    s
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

    /// User `user_id` types `text` as the reason for what they chose.
    async fn typed(&self, user_id: i64, text: &str) -> (Vec<Message>, Vec<i64>) {
        let input = Input::Text(text.to_owned());
        self.handle(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<ReasonFlow>());
            let session = session.expect("giving a reason");
            let answered = answer(turn, &self.config, session, &input).await?;
            Ok(answered.expect("an answer"))
        })
        .await
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        self.typed(user_id, text).await.0
    }

    /// Files a report on `target` by `reporter`, or by moderation, `ago` before `t0`.
    async fn report(&self, reporter: Option<i64>, target: Target, reason: &str, ago: TimeDelta) {
        let (target_type, target_id) = target.stored();
        report::ActiveModel {
            reporter_id: Set(reporter),
            target_type: Set(target_type),
            target_id: Set(target_id),
            reason: Set(reason.to_owned()),
            status: Set(report::Status::Open),
            resolved_by_id: Set(None),
            resolved_at: Set(None),
            created_at: Set(t0() - ago),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .unwrap();
    }

    async fn product(&self, id: i64) -> product::Model {
        product::Entity::find_by_id(id)
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

    /// The postback of `action` on the card of `target` as it is now.
    async fn button(&self, action: &str, target: Target) -> String {
        let (key, version) = match target {
            Target::Product(id) => ("p", self.product(id).await.updated_at),
            Target::Shop(id) => (
                "s",
                shop::Entity::find_by_id(id)
                    .one(&self.db)
                    .await
                    .unwrap()
                    .unwrap()
                    .updated_at,
            ),
        };
        let id = match target {
            Target::Product(id) | Target::Shop(id) => id,
        };
        let latest = open(target)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|report| report.id)
            .max()
            .unwrap_or_default();
        format!(
            "{action}?{key}={id}&v={}&r={latest}",
            version.timestamp_micros()
        )
    }

    async fn in_flow(&self, user_id: i64) -> bool {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .is_some()
    }

    /// Every report, as (reporter, target, status, resolved by, resolved at), oldest first.
    #[allow(clippy::type_complexity)]
    async fn reports(
        &self,
    ) -> Vec<(
        Option<i64>,
        Target,
        report::Status,
        Option<i64>,
        Option<DateTimeUtc>,
    )> {
        report::Entity::find()
            .order_by_asc(report::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|report| {
                let target = match report.target_type {
                    report::TargetType::Product => Target::Product(report.target_id),
                    report::TargetType::Shop => Target::Shop(report.target_id),
                };
                (
                    report.reporter_id,
                    target,
                    report.status,
                    report.resolved_by_id,
                    report.resolved_at,
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

/// NFR-SEC-02: only admins see the queue or decide.
#[tokio::test]
async fn only_admins_review() {
    let s = setup().await;
    let approve = s.button(APPROVE, Target::Product(s.held.id)).await;
    for data in ["review_queue", approve.as_str()] {
        assert_eq!(
            s.taps(s.seller.id, data).await,
            [s.templates.denied(Denied::NotAllowed)],
            "{data}"
        );
    }
    assert_eq!(s.product(s.held.id).await.status, product::Status::Held);
    assert_eq!((s.pushes().await, s.audited().await), (vec![], vec![]));
}

/// FR-ADM-03: a card for each product or shop with open reports, the one waiting longest
/// first, with its state, its latest reports and who made them, and the buttons its state
/// allows; paged.
#[tokio::test]
async fn the_queue_lists_what_waits_longest_first() {
    let s = setup().await;
    let (held, listed) = (Target::Product(s.held.id), Target::Product(s.listed.id));
    // A second report on the held product, after the listed one's: it stays first
    s.report(
        Some(s.buyer.id),
        held,
        "หลอกลวง/โกง",
        TimeDelta::minutes(30),
    )
    .await;
    let (approve, remove, suspend) = (
        s.button(APPROVE, held).await,
        s.button(REMOVE, held).await,
        s.button(SUSPEND, held).await,
    );
    let listed_buttons = [
        s.button(REMOVE, listed).await,
        s.button(SUSPEND, listed).await,
        s.button(DISMISS, listed).await,
    ];
    let first = s.taps(s.admin, "review_queue").await;
    let button = |data: &str, label: &Label| Postback::parse(data).unwrap().button(label.clone());
    let cards = [
        ReviewCard {
            what: Reported::Product {
                name: "ไข่ไก่เบอร์ 2",
                shop: "ร้านป้าแดง",
            },
            photo: None,
            state: Reviewed::Product {
                status: product::Status::Held,
                shop: shop::Status::Approved,
            },
            reports: vec![
                ReviewReport {
                    by: Reporter::User(Some("สมชาย")),
                    reason: "หลอกลวง/โกง",
                    at: t0() - TimeDelta::minutes(30),
                },
                ReviewReport {
                    by: Reporter::Moderation,
                    reason: HELD_FOR,
                    at: t0() - TimeDelta::hours(2),
                },
            ],
            buttons: vec![
                button(&approve, &labels::APPROVE),
                button(&remove, &labels::REMOVE_PRODUCT),
                button(&suspend, &labels::SUSPEND_SHOP),
            ],
        },
        ReviewCard {
            what: Reported::Product {
                name: "ขนมครก",
                shop: "ร้านป้าแดง",
            },
            photo: None,
            state: Reviewed::Product {
                status: product::Status::Active,
                shop: shop::Status::Approved,
            },
            reports: vec![ReviewReport {
                by: Reporter::User(Some("สมชาย")),
                reason: "ข้อมูลไม่ตรง",
                at: t0() - TimeDelta::hours(1),
            }],
            buttons: vec![
                button(&listed_buttons[0], &labels::REMOVE_PRODUCT),
                button(&listed_buttons[1], &labels::SUSPEND_SHOP),
                button(&listed_buttons[2], &labels::DISMISS),
            ],
        },
    ];
    let expected = s.templates.review_queue(&cards, 0, |_| unreachable!());
    assert_eq!(first, [expected]);
    assert_eq!(
        quick_replies(&first[0]),
        [labels::REVIEW_QUEUE.as_str(), "เมนู"]
    );

    // A held shop: approved or suspended, not dismissed; and a page more
    let mut held_shop = Vec::new();
    for i in 1..templates::REVIEWS_PER_PAGE {
        let owner = testing::new_user(&format!("U-owner-{i}"));
        let owner = owner.insert(&s.db).await.unwrap();
        let shop = shop::ActiveModel {
            status: Set(shop::Status::Held),
            name: Set(format!("ร้านที่ {i}")),
            ..testing::new_shop(owner.id)
        };
        let shop = shop.insert(&s.db).await.unwrap();
        s.report(
            None,
            Target::Shop(shop.id),
            HELD_FOR,
            TimeDelta::minutes(10),
        )
        .await;
        held_shop.push(shop);
    }
    let first = s.taps(s.admin, "review_queue").await;
    let cards = card_buttons(&first[0]);
    assert_eq!(cards.len(), templates::REVIEWS_PER_PAGE + 1);
    let shop = Target::Shop(held_shop[0].id);
    assert_eq!(
        cards[2],
        [s.button(APPROVE, shop).await, s.button(SUSPEND, shop).await]
    );
    assert!(
        cards.last().unwrap().is_empty(),
        "\"ดูเพิ่มเติม\" has no footer"
    );
    let last = Target::Shop(held_shop.last().unwrap().id);
    let second = s.taps(s.admin, "review_queue?page=1").await;
    assert_eq!(
        card_buttons(&second[0]),
        [[s.button(APPROVE, last).await, s.button(SUSPEND, last).await]]
    );
    // A page past the last starts over
    assert_eq!(s.taps(s.admin, "review_queue?page=9").await, first);

    // Nothing open, nothing listed
    report::Entity::update_many()
        .set(report::ActiveModel {
            status: Set(report::Status::Dismissed),
            ..Default::default()
        })
        .filter(report::Column::Status.eq(report::Status::Open))
        .exec(&s.db)
        .await
        .unwrap();
    let none = s.taps(s.admin, "review_queue").await;
    assert!(text(&none[0]).starts_with("ตอนนี้ไม่มีรายการรอตรวจ"));
}

/// Acceptance: approving a held product makes it `ACTIVE`, so buyers see it; its reports are
/// resolved, and it is audited. Its seller isn't pushed (Table 4.8-A).
#[tokio::test]
async fn approving_a_held_product_lists_it() {
    let s = setup().await;
    let approve = s.button(APPROVE, Target::Product(s.held.id)).await;
    let (reply, relink) = s.tapped(s.admin, &approve).await;
    let what = Reported::Product {
        name: "ไข่ไก่เบอร์ 2",
        shop: "ร้านป้าแดง",
    };
    assert_eq!(reply, [s.templates.review_approved(&what)]);
    assert_eq!(text(&reply[0]), "อนุมัติสินค้า \"ไข่ไก่เบอร์ 2\" แล้วค่ะ");
    assert!(relink.is_empty(), "{relink:?}");

    let product = s.product(s.held.id).await;
    assert_eq!(
        (product.status, product.updated_at),
        (product::Status::Active, t0())
    );
    assert!(listing::product(&s.db, s.held.id).await.unwrap().is_some());
    let reports = s.reports().await;
    assert_eq!(
        reports[0],
        (
            None,
            Target::Product(s.held.id),
            report::Status::Resolved,
            Some(s.admin),
            Some(t0())
        )
    );
    assert_eq!(reports[1].2, report::Status::Open, "another product's");
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin),
            "product.approved".to_owned(),
            "product".to_owned(),
            Some(s.held.id),
            json!({ "name": "ไข่ไก่เบอร์ 2", "shop": "ร้านป้าแดง", "reports": [1] })
        )]
    );
    assert_eq!(s.pushes().await, []);
}

/// Acceptance: removing a product asks why, and tells its seller with the reason; buyers never
/// see it again. A product reported while listed can be removed too (FR-ADM-03).
#[tokio::test]
async fn removing_a_product_tells_its_seller_why() {
    let s = setup().await;
    let remove = s.button(REMOVE, Target::Product(s.held.id)).await;
    let asked = s.taps(s.admin, &remove).await;
    assert!(
        text(&asked[0]).starts_with("นำสินค้า \"ไข่ไก่เบอร์ 2\" ของร้านป้าแดงออกเพราะอะไรคะ"),
        "{}",
        text(&asked[0])
    );
    assert_eq!(
        quick_replies(&asked[0]),
        ["สินค้าต้องห้าม", "ข้อมูลไม่ตรง", "หลอกลวง/โกง", "ไม่สุภาพ", "ยกเลิก"]
    );
    assert!(s.in_flow(s.admin).await);
    // Nothing is decided until the reason is given
    assert_eq!(s.product(s.held.id).await.status, product::Status::Held);
    let too_long = "ก".repeat(REASON_CHARS + 1);
    assert_eq!(
        s.types(s.admin, &too_long).await,
        [templates::with_cancel(
            s.templates.text_too_long(REASON_CHARS)
        )]
    );

    let reply = s.types(s.admin, "สินค้าต้องห้าม").await;
    assert_eq!(reply, [s.templates.product_removed("ไข่ไก่เบอร์ 2")]);
    assert!(!s.in_flow(s.admin).await);
    let product = s.product(s.held.id).await;
    assert_eq!(
        (product.status, product.updated_at),
        (product::Status::Removed, t0())
    );
    assert_eq!(s.reports().await[0].2, report::Status::Resolved);
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin),
            "product.removed".to_owned(),
            "product".to_owned(),
            Some(s.held.id),
            json!({
                "name": "ไข่ไก่เบอร์ 2",
                "shop": "ร้านป้าแดง",
                "reports": [1],
                "reason": "สินค้าต้องห้าม",
            })
        )]
    );
    let my_products = Item::MyProducts
        .postback()
        .button(labels::MY_PRODUCTS.clone());
    let removed = s
        .templates
        .product_removed_notice("ไข่ไก่เบอร์ 2", "สินค้าต้องห้าม", my_products);
    assert_eq!(
        s.pushes().await,
        [(
            "product.removed".to_owned(),
            SELLER.to_owned(),
            queued(&[removed])
        )]
    );

    // Listed and reported
    assert!(
        listing::product(&s.db, s.listed.id)
            .await
            .unwrap()
            .is_some()
    );
    s.taps(
        s.admin,
        &s.button(REMOVE, Target::Product(s.listed.id)).await,
    )
    .await;
    s.types(s.admin, "ข้อมูลไม่ตรง").await;
    let product = s.product(s.listed.id).await;
    assert_eq!(product.status, product::Status::Removed);
    assert!(
        listing::product(&s.db, s.listed.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(s.pushes().await.len(), 2);
}

/// FR-ADM-03, FR-SHOP-10: suspending a shop asks why; buyers see neither it nor its products,
/// its owner is told why, loses the seller's menu, and hears the reason whenever refused.
#[tokio::test]
async fn suspending_a_shop_hides_it_and_tells_its_owner() {
    let s = setup().await;
    let listed = Target::Product(s.listed.id);
    let asked = s.taps(s.admin, &s.button(SUSPEND, listed).await).await;
    assert!(
        text(&asked[0]).starts_with("ระงับร้านป้าแดงเพราะอะไรคะ"),
        "{}",
        text(&asked[0])
    );
    let (reply, relink) = s.typed(s.admin, "ขายของปลอม").await;
    assert_eq!(reply, [s.templates.shop_suspended("ร้านป้าแดง")]);
    assert_eq!(relink, [s.seller.id]);

    let shop = s.the_shop().await;
    assert_eq!(
        (
            shop.status,
            shop.suspension_reason.as_deref(),
            shop.updated_at
        ),
        (shop::Status::Suspended, Some("ขายของปลอม"), t0())
    );
    assert_eq!(s.product(s.listed.id).await.status, product::Status::Active);
    assert!(
        listing::product(&s.db, s.listed.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(listing::shop(&s.db, s.shop.id).await.unwrap().is_none());
    let reports = s.reports().await;
    assert_eq!(
        (reports[0].2, reports[1].2),
        (report::Status::Open, report::Status::Resolved)
    );
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin),
            "shop.suspended".to_owned(),
            "shop".to_owned(),
            Some(s.shop.id),
            json!({ "name": "ร้านป้าแดง", "reports": [2], "reason": "ขายของปลอม" })
        )]
    );
    let suspended = s.templates.shop_suspended_notice("ร้านป้าแดง", "ขายของปลอม");
    assert_eq!(
        s.pushes().await,
        [(
            "shop.suspended".to_owned(),
            SELLER.to_owned(),
            queued(&[suspended])
        )]
    );
    let actor = Actor::load(&s.db, s.seller.id).await.unwrap().unwrap();
    assert!(!actor.roles().seller);
    let (refused, _) = s
        .handle(s.seller.id, async |turn| {
            Ok(vec![turn.denied(Denied::ShopSuspended)])
        })
        .await;
    assert!(
        text(&refused[0]).contains("ร้านของคุณถูกระงับชั่วคราว เหตุผล: ขายของปลอม"),
        "{}",
        text(&refused[0])
    );

    // The held product's card no longer offers to suspend its shop
    let held = Target::Product(s.held.id);
    let queue = s.taps(s.admin, "review_queue").await;
    assert_eq!(
        card_buttons(&queue[0]),
        [[s.button(APPROVE, held).await, s.button(REMOVE, held).await]]
    );
}

/// FR-ADM-03: dismissing closes the reports and leaves what they were about as it is; approving
/// a held shop shows it to buyers again.
#[tokio::test]
async fn dismissing_or_approving_a_shop_leaves_it_selling() {
    let s = setup().await;
    let listed = Target::Product(s.listed.id);
    let reply = s.taps(s.admin, &s.button(DISMISS, listed).await).await;
    let what = Reported::Product {
        name: "ขนมครก",
        shop: "ร้านป้าแดง",
    };
    assert_eq!(reply, [s.templates.reports_dismissed(&what)]);
    assert_eq!(s.product(s.listed.id).await.status, product::Status::Active);
    assert_eq!(
        s.reports().await[1],
        (
            Some(s.buyer.id),
            listed,
            report::Status::Dismissed,
            Some(s.admin),
            Some(t0())
        )
    );

    let mut held = s.shop.clone().into_active_model();
    held.status = Set(shop::Status::Held);
    held.update(&s.db).await.unwrap();
    s.report(
        None,
        Target::Shop(s.shop.id),
        HELD_FOR,
        TimeDelta::minutes(5),
    )
    .await;
    assert!(listing::shop(&s.db, s.shop.id).await.unwrap().is_none());
    let approve = s.button(APPROVE, Target::Shop(s.shop.id)).await;
    let (reply, relink) = s.tapped(s.admin, &approve).await;
    let what = Reported::Shop {
        name: "ร้านป้าแดง"
    };
    assert_eq!(reply, [s.templates.review_approved(&what)]);
    assert!(relink.is_empty(), "a held shop's owner is a seller already");
    assert_eq!(s.the_shop().await.status, shop::Status::Approved);
    assert!(listing::shop(&s.db, s.shop.id).await.unwrap().is_some());
    assert_eq!(
        s.audited().await,
        [
            (
                Some(s.admin),
                "report.dismissed".to_owned(),
                "product".to_owned(),
                Some(s.listed.id),
                json!({ "name": "ขนมครก", "shop": "ร้านป้าแดง", "reports": [2] })
            ),
            (
                Some(s.admin),
                "shop.approved".to_owned(),
                "shop".to_owned(),
                Some(s.shop.id),
                json!({ "name": "ร้านป้าแดง", "reports": [3] })
            ),
        ]
    );
    assert_eq!(s.pushes().await, []);
}

/// FR-UI-13: a second tap, or another admin's, on a card already dealt with does nothing more
/// and shows the queue; a card of a product edited since shows it as it is now, with buttons for
/// it; and a report filed since the card was sent stays open.
#[tokio::test]
async fn a_card_that_no_longer_matches_shows_it_as_it_is() {
    let s = setup().await;
    let held = Target::Product(s.held.id);
    let (approve, remove) = (s.button(APPROVE, held).await, s.button(REMOVE, held).await);
    s.taps(s.admin, &approve).await;
    let handled = s.taps(s.admin, &approve).await;
    assert_eq!(handled[0], s.templates.review_handled());
    assert_eq!(handled[1], s.taps(s.admin, "review_queue").await[0]);
    assert_eq!(s.taps(s.other_admin, &remove).await, handled);
    assert!(!s.in_flow(s.other_admin).await);
    assert_eq!(s.product(s.held.id).await.status, product::Status::Active);
    assert_eq!(s.audited().await.len(), 1);

    // Edited by its seller since its card was sent
    let listed = Target::Product(s.listed.id);
    let (remove, dismiss) = (
        s.button(REMOVE, listed).await,
        s.button(DISMISS, listed).await,
    );
    let mut edited = s.listed.clone().into_active_model();
    edited.description = Set(Some("ทำสดทุกเช้า".to_owned()));
    edited.updated_at = Set(t0() - TimeDelta::minutes(1));
    edited.update(&s.db).await.unwrap();
    let changed = s.taps(s.admin, &remove).await;
    assert_eq!(changed[0], s.templates.review_changed());
    assert_eq!(
        card_buttons(&changed[1]),
        [[
            s.button(REMOVE, listed).await,
            s.button(SUSPEND, listed).await,
            s.button(DISMISS, listed).await,
        ]]
    );
    assert!(!s.in_flow(s.admin).await);
    assert_eq!(s.product(s.listed.id).await.status, product::Status::Active);

    // Reported again after the card was sent: dismissing closes only what it showed
    let dismiss_shown = s.button(DISMISS, listed).await;
    assert_ne!(dismiss_shown, dismiss);
    s.report(None, listed, HELD_FOR, TimeDelta::seconds(1))
        .await;
    s.taps(s.admin, &dismiss_shown).await;
    let statuses: Vec<_> = s.reports().await.into_iter().map(|row| row.2).collect();
    assert_eq!(
        statuses,
        [
            report::Status::Resolved,
            report::Status::Dismissed,
            report::Status::Open
        ]
    );
    let queue = s.taps(s.admin, "review_queue").await;
    assert_eq!(card_buttons(&queue[0]).len(), 1);

    // Gone, or never there
    let missing = format!("{DISMISS}?p={}&v=1&r=1", s.listed.id + 100);
    let gone = s.taps(s.admin, &missing).await;
    assert_eq!(gone[0], s.templates.denied(Denied::NotFound));
    assert_eq!(
        s.taps(s.admin, "review.dismiss?p=1").await,
        [s.templates.denied(Denied::NotFound)]
    );
}

/// The reason is typed after the decision is chosen: another admin may decide meanwhile, or the
/// admin lose their role, and then nothing is removed or suspended.
#[tokio::test]
async fn nothing_is_decided_once_it_no_longer_can_be() {
    let s = setup().await;
    let held = Target::Product(s.held.id);
    s.taps(s.admin, &s.button(REMOVE, held).await).await;
    s.taps(s.other_admin, &s.button(APPROVE, held).await).await;
    let reply = s.types(s.admin, "สินค้าต้องห้าม").await;
    assert_eq!(reply[0], s.templates.review_handled());
    assert!(!s.in_flow(s.admin).await);
    assert_eq!(s.product(s.held.id).await.status, product::Status::Active);

    s.taps(
        s.admin,
        &s.button(SUSPEND, Target::Product(s.listed.id)).await,
    )
    .await;
    let mut demoted = user::Entity::find_by_id(s.admin)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    demoted.is_admin = Set(false);
    demoted.update(&s.db).await.unwrap();
    assert_eq!(
        s.types(s.admin, "ขายของปลอม").await,
        [s.templates.denied(Denied::NotAllowed)]
    );
    assert!(!s.in_flow(s.admin).await);
    assert_eq!(s.the_shop().await.status, shop::Status::Approved);
    let decided: Vec<_> = s
        .audited()
        .await
        .into_iter()
        .map(|(_, action, ..)| action)
        .collect();
    assert_eq!(decided, ["product.approved"]);
}
