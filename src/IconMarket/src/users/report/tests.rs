//! Problem reports (FR-HELP-02): a report on a product or shop is filed with its reason and
//! alerts the admins when `notification.adminAlerts` is on; what can't be reported, by whom,
//! is refused; and a second report of the same thing while the first is open alerts no one.
//! Each event is handled in a transaction committed once it is handled, with what it notifies
//! queued in it. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{ActiveModelTrait, DatabaseConnection, QueryOrder, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, Config, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::{Body, limits};
use crate::notifications::templates::{self, Templates};
use crate::persistence::entity::{push_log, session, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

#[test]
fn reasons_are_tidied_and_bounded() {
    assert_eq!(reason("  ไข่เน่า \n ทั้งแผง ").as_deref(), Ok("ไข่เน่า ทั้งแผง"));
    let longest = "ก".repeat(REASON_CHARS);
    assert_eq!(reason(&longest), Ok(longest.clone()));
    assert_eq!(
        reason(&format!("{longest}ก")),
        Err(Refused::TooLong(REASON_CHARS))
    );
    assert_eq!(reason(" \t\n "), Err(Refused::Empty));
}

#[test]
fn a_button_names_what_it_reports() {
    for target in [Target::Product(12), Target::Shop(3)] {
        let Action::Postback { label, data, .. } = button(target) else {
            panic!("not a postback");
        };
        assert_eq!(label.as_str(), "แจ้งปัญหา");
        assert_eq!(Target::of(&Postback::parse(&data).unwrap()), Some(target));
    }
    assert_eq!(button(Target::Product(12)).label(), &labels::REPORT);
    for data in ["report", "report?p=1&s=2", "report?p=x", "report?c=1"] {
        assert_eq!(Target::of(&Postback::parse(data).unwrap()), None, "{data}");
    }
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    /// A buyer, and a shop with one product, of the seller's
    market: testing::Market,
    seller: i64,
    admin: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = testing::market(&db).await;
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(ADMIN)
    };
    let admin = admin.insert(&db).await.unwrap().id;
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        seller: market.shop.owner_id,
        market,
        admin,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s with `handle`, as the worker does: in a transaction
    /// that commits once it is handled, with the user loaded afresh and what it notifies queued.
    async fn at(
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
        let Finished {
            messages, outbox, ..
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.config.notification, t0())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        messages
    }

    /// User `user_id` taps a button sending `data`.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.at(user_id, async |turn| tapped(turn, &postback).await)
            .await
    }

    /// User `user_id` answers their report flow with `input`; `None` if the step doesn't take
    /// it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let mut answered = None;
        self.at(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<ReportFlow>());
            answered = answer(turn, session.expect("in the report flow"), &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(user_id, input).await.expect("an answer")
    }

    fn buyer(&self) -> i64 {
        self.market.buyer.id
    }

    fn product(&self) -> String {
        format!("report?p={}", self.market.product.id)
    }

    fn shop(&self) -> String {
        format!("report?s={}", self.market.shop.id)
    }

    fn the_product(&self) -> Reported<'_> {
        Reported::Product {
            name: &self.market.product.name,
            shop: &self.market.shop.name,
        }
    }

    fn the_shop(&self) -> Reported<'_> {
        Reported::Shop {
            name: &self.market.shop.name,
        }
    }

    /// What asks why `what` is reported, with the cancel button.
    fn asked(&self, what: &Reported<'_>) -> Message {
        let reasons = REASONS
            .iter()
            .map(|&label| Action::message(label.clone(), label.as_str()))
            .collect();
        templates::with_cancel(
            self.templates
                .ask_report_reason(what, REASON_CHARS, reasons),
        )
    }

    /// What filing a report on `what` answers user `user_id`.
    async fn received(&self, user_id: i64, what: &Reported<'_>) -> Vec<Message> {
        let actor = Actor::load(&self.db, user_id).await.unwrap().unwrap();
        vec![
            self.templates.report_received(what),
            menu::main_menu(&actor),
        ]
    }

    /// The admins' alert of a report on `what` for `reason`.
    fn alert(&self, what: &Reported<'_>, reason: &str) -> Message {
        let review = Action::Postback {
            label: labels::REVIEW_QUEUE.clone(),
            data: "review_queue".to_owned(),
            display_text: Some("รายการรอตรวจ".to_owned()),
        };
        self.templates.report_alert(what, reason, t0(), review)
    }

    /// Every report, as (reporter, target, reason, status), oldest first.
    async fn reports(&self) -> Vec<(Option<i64>, Target, String, report::Status)> {
        report::Entity::find()
            .order_by_asc(report::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                assert_eq!(row.created_at, t0());
                assert_eq!((row.resolved_by_id, row.resolved_at), (None, None));
                let target = match row.target_type {
                    report::TargetType::Product => Target::Product(row.target_id),
                    report::TargetType::Shop => Target::Shop(row.target_id),
                };
                (row.reporter_id, target, row.reason, row.status)
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

    async fn in_flow(&self, user_id: i64) -> bool {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .is_some()
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

/// Acceptance: a report creates a Report entry and an admin alert.
#[tokio::test]
async fn a_product_report_is_filed_and_the_admins_alerted() {
    let s = setup().await;
    let buyer = s.buyer();
    let what = s.the_product();

    let asked = s.taps(buyer, &s.product()).await;
    assert_eq!(asked, [s.asked(&what)]);
    assert!(text(&asked[0]).contains("สินค้า \"ไข่ไก่เบอร์ 2\" ของร้านป้าแดง"));
    limits::check(&asked).unwrap();
    assert!(s.in_flow(buyer).await);
    // Nothing is filed until the reason is given
    assert_eq!(s.reports().await, []);

    let filed = s.types(buyer, "  ไข่เน่า \n ทั้งแผง ").await;
    assert_eq!(filed, s.received(buyer, &what).await);
    assert!(!s.in_flow(buyer).await);
    let reason = "ไข่เน่า ทั้งแผง";
    assert_eq!(
        s.reports().await,
        [(
            Some(buyer),
            Target::Product(s.market.product.id),
            reason.to_owned(),
            report::Status::Open
        )]
    );
    let alert = s.alert(&what, reason);
    limits::check(std::slice::from_ref(&alert)).unwrap();
    assert_eq!(
        s.pushes().await,
        [(
            "admin.user_report".to_owned(),
            ADMIN.to_owned(),
            queued(&[alert])
        )]
    );
}

/// A shop is reported likewise, and a reason offered is taken as if typed.
#[tokio::test]
async fn a_shop_report_takes_a_reason_offered() {
    let s = setup().await;
    let buyer = s.buyer();
    let what = s.the_shop();
    assert_eq!(s.taps(buyer, &s.shop()).await, [s.asked(&what)]);
    let filed = s.types(buyer, labels::REASON_SCAM.as_str()).await;
    assert_eq!(filed, s.received(buyer, &what).await);
    assert_eq!(
        s.reports().await,
        [(
            Some(buyer),
            Target::Shop(s.market.shop.id),
            "หลอกลวง/โกง".to_owned(),
            report::Status::Open
        )]
    );
    let alert = s.alert(&what, "หลอกลวง/โกง");
    let pushes = s.pushes().await;
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].2, queued(&[alert]));
}

/// A reason must be given, and fit; a photo is no answer; cancelling files nothing.
#[tokio::test]
async fn the_step_waits_for_a_reason() {
    let s = setup().await;
    let buyer = s.buyer();
    let what = s.the_product();
    s.taps(buyer, &s.product()).await;

    assert_eq!(s.types(buyer, " \n ").await, [s.asked(&what)]);
    let too_long = "ก".repeat(REASON_CHARS + 1);
    assert_eq!(
        s.types(buyer, &too_long).await,
        [templates::with_cancel(
            s.templates.text_too_long(REASON_CHARS)
        )]
    );
    let photo = Input::Image {
        message_id: "1".to_owned(),
    };
    assert_eq!(s.sends(buyer, photo).await, None);
    assert!(s.in_flow(buyer).await);
    assert_eq!(s.reports().await, []);

    s.at(buyer, async |turn| flow::cancel(turn).await).await;
    assert!(!s.in_flow(buyer).await);
    assert_eq!((s.reports().await, s.pushes().await), (vec![], vec![]));

    // The longest reason is taken whole
    s.taps(buyer, &s.product()).await;
    let longest = "ก".repeat(REASON_CHARS);
    s.types(buyer, &longest).await;
    assert_eq!(s.reports().await[0].2, longest);
}

/// A second report of the same thing by the same user, while the first is open, is neither
/// filed nor alerted; someone else's is, and so is theirs once the first is dealt with.
#[tokio::test]
async fn one_open_report_per_user_and_target() {
    let s = setup().await;
    let buyer = s.buyer();
    let what = s.the_product();
    s.taps(buyer, &s.product()).await;
    s.types(buyer, "ของปลอม").await;

    assert_eq!(
        s.taps(buyer, &s.product()).await,
        [s.templates.already_reported(&what)]
    );
    assert!(!s.in_flow(buyer).await);
    // A flow started before the first was filed finds it filed
    s.at(buyer, async |turn| {
        let session = Session {
            step: Step::Reason,
            draft: ReportFlow {
                target: Target::Product(s.market.product.id),
            },
        };
        flow::start(turn, session, vec![s.asked(&what)]).await
    })
    .await;
    assert_eq!(
        s.types(buyer, "ของปลอมจริงๆ").await,
        [s.templates.already_reported(&what)]
    );
    assert!(!s.in_flow(buyer).await);
    // Their shop is another target
    s.taps(buyer, &s.shop()).await;
    s.types(buyer, "ไม่สุภาพ").await;
    // And the admin is another user
    s.taps(s.admin, &s.product()).await;
    s.types(s.admin, "ข้อมูลไม่ตรง").await;
    assert_eq!(s.reports().await.len(), 3);
    assert_eq!(s.pushes().await.len(), 3);

    // Once dismissed, the buyer can report the product again
    let first = report::Entity::find()
        .order_by_asc(report::Column::Id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    let dismissed = report::ActiveModel {
        status: Set(report::Status::Dismissed),
        ..first.into()
    };
    dismissed.update(&s.db).await.unwrap();
    assert_eq!(s.taps(buyer, &s.product()).await, [s.asked(&what)]);
}

/// What isn't there to report, or is the user's own, is refused, and no flow starts.
#[tokio::test]
async fn what_cannot_be_reported_is_refused() {
    let s = setup().await;
    let buyer = s.buyer();
    let not_found = vec![s.templates.denied(Denied::NotFound)];
    let refused = |data: String, expected: Vec<Message>| {
        let s = &s;
        async move {
            assert_eq!(s.taps(buyer, &data).await, expected, "{data}");
            assert!(!s.in_flow(buyer).await, "{data}");
        }
    };
    refused("report?p=999".to_owned(), not_found.clone()).await;
    refused("report?s=999".to_owned(), not_found.clone()).await;
    refused("report".to_owned(), not_found.clone()).await;
    refused(
        format!("report?p={}&s=1", s.market.product.id),
        not_found.clone(),
    )
    .await;

    let product = s.market.product.clone();
    for status in [product::Status::Draft, product::Status::Deleted] {
        let changed = product::ActiveModel {
            status: Set(status),
            ..product.clone().into()
        };
        changed.update(&s.db).await.unwrap();
        refused(s.product(), not_found.clone()).await;
    }
    // Held, hidden or removed products can still be reported
    let held = product::ActiveModel {
        status: Set(product::Status::Held),
        ..product.into()
    };
    held.update(&s.db).await.unwrap();
    assert_eq!(
        s.taps(buyer, &s.product()).await,
        [s.asked(&s.the_product())]
    );
    s.at(buyer, async |turn| flow::cancel(turn).await).await;

    for status in [
        shop::Status::Pending,
        shop::Status::Rejected,
        shop::Status::Closed,
    ] {
        let changed = shop::ActiveModel {
            status: Set(status),
            ..s.market.shop.clone().into()
        };
        changed.update(&s.db).await.unwrap();
        refused(s.shop(), not_found.clone()).await;
    }
    let suspended = shop::ActiveModel {
        status: Set(shop::Status::Suspended),
        ..s.market.shop.clone().into()
    };
    suspended.update(&s.db).await.unwrap();
    assert_eq!(s.taps(buyer, &s.shop()).await, [s.asked(&s.the_shop())]);

    // A seller can't report their own shop or products
    let own = vec![s.templates.denied(Denied::NotAllowed)];
    for data in [s.shop(), s.product()] {
        assert_eq!(s.taps(s.seller, &data).await, own, "{data}");
        assert!(!s.in_flow(s.seller).await);
    }
    assert_eq!(s.reports().await, []);
}

/// FR-ADM-04: a suspended user can't report, even mid-flow.
#[tokio::test]
async fn suspended_users_cannot_report() {
    let s = setup().await;
    let buyer = s.buyer();
    s.taps(buyer, &s.product()).await;
    let suspend = |status| user::ActiveModel {
        status: Set(status),
        ..s.market.buyer.clone().into()
    };
    suspend(user::Status::Suspended)
        .update(&s.db)
        .await
        .unwrap();

    let denied = vec![s.templates.denied(Denied::Suspended)];
    assert_eq!(s.types(buyer, "ของปลอม").await, denied);
    assert!(!s.in_flow(buyer).await);
    assert_eq!(s.taps(buyer, &s.shop()).await, denied);
    assert!(!s.in_flow(buyer).await);
    assert_eq!((s.reports().await, s.pushes().await), (vec![], vec![]));
}

/// Table 4.8-A: with `notification.adminAlerts` off, the report is filed and no one is
/// alerted.
#[tokio::test]
async fn the_alert_follows_its_switch() {
    let mut s = setup().await;
    s.config.notification.admin_alerts = false;
    let buyer = s.buyer();
    s.taps(buyer, &s.product()).await;
    let filed = s.types(buyer, "ของปลอม").await;
    assert_eq!(filed, s.received(buyer, &s.the_product()).await);
    assert_eq!(s.reports().await.len(), 1);
    assert_eq!(s.pushes().await, []);
}
