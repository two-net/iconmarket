//! The daily sales summary (FR-NOT-06) against a database, a fake LINE and a scripted AI: only
//! while switched on and only to sellers who follow the OA (the acceptance of #47), only to those
//! who may see their sales and had a day worth telling; the day it counts, pre-orders handed over
//! included; the AI's comment, or none; and no seller pushed twice a day, nor at night. Each test
//! starts a throwaway PostgreSQL, so they need Docker.

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{TimeDelta, TimeZone};
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::ai::testing::{self as scripted_ai, Model, Turn};
use crate::ai::{Part, Role};
use crate::config;
use crate::line::Message;
use crate::line::testing::{Fake, PUSH};
use crate::persistence::entity::order::Status::{Accepted, Completed, Pending, Rejected};
use crate::persistence::entity::{ai_call_log, order_item, product, push_log};
use crate::persistence::{orm, random_id, testing};

const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const OTHER_SELLER: &str = "Ue5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";
const BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

/// ขนมครก sells at ฿20 a กล่อง
const PRICE: i64 = 20;

fn bangkok(month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    chrono_tz::Asia::Bangkok
        .with_ymd_and_hms(2026, month, day, hour, minute, 0)
        .unwrap()
        .with_timezone(&Utc)
}

/// 30 ต.ค. 2569 20:00 in Bangkok: the summary's time, `notification.dailySummaryTime`
fn eight_pm() -> DateTime<Utc> {
    bangkok(10, 30, 20, 0)
}

/// 30 ต.ค. 00:00 in Bangkok, when the day summarized began
fn midnight() -> DateTime<Utc> {
    bangkok(10, 30, 0, 0)
}

fn hours(hours: i64) -> TimeDelta {
    TimeDelta::hours(hours)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    templates: Templates,
    notification: NotificationConfig,
    buyer: i64,
    /// The seller's shop and its one product, ขนมครก
    shop: i64,
    product: i64,
    category: i64,
    numbers: AtomicU32,
}

/// A buyer, and the seller with an approved shop selling ขนมครก, with no orders yet; the daily
/// summary switched on.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool.clone());
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap();
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&db).await.unwrap();
    let category = testing::new_category("ขนม/ของหวาน")
        .insert(&db)
        .await
        .unwrap();
    let product = Setup::new_product(&db, shop.id, category.id).await;
    let config = config::sample();
    let notification = NotificationConfig {
        daily_summary: true,
        ..config.notification
    };
    assert_eq!(notification.daily_summary_time.to_string(), "20:00:00");
    Setup {
        _container: container,
        pool,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        notification,
        buyer: buyer.id,
        shop: shop.id,
        product,
        category: category.id,
        numbers: AtomicU32::new(1),
    }
}

impl Setup {
    async fn new_product(db: &DatabaseConnection, shop: i64, category: i64) -> i64 {
        let product = product::ActiveModel {
            price: Set(Money::from_baht(PRICE)),
            unit: Set("กล่อง".to_owned()),
            ..testing::new_product(shop, category, "ขนมครก", None)
        };
        product.insert(db).await.unwrap().id
    }

    /// Another seller, `line_user_id`, with a shop of `status` selling ขนมครก. Returns the shop
    /// and its product.
    async fn other_shop(&self, line_user_id: &str, status: shop::Status) -> (i64, i64) {
        let seller = testing::new_user(line_user_id)
            .insert(&self.db)
            .await
            .unwrap();
        let shop = shop::ActiveModel {
            name: Set(format!("ร้าน {line_user_id}")),
            status: Set(status),
            ..testing::new_shop(seller.id)
        }
        .insert(&self.db)
        .await
        .unwrap();
        (
            shop.id,
            Self::new_product(&self.db, shop.id, self.category).await,
        )
    }

    /// An order at `shop` of `quantity` กล่อง of its `product`, placed at `placed`, completed at
    /// `completed` if it was, and now at `status`.
    async fn order(
        &self,
        (shop, product): (i64, i64),
        quantity: i32,
        placed: DateTime<Utc>,
        completed: Option<DateTime<Utc>>,
        status: order::Status,
    ) {
        let number = self.numbers.fetch_add(1, Ordering::Relaxed);
        let total = Money::from_baht(PRICE * i64::from(quantity));
        let order = order::ActiveModel {
            status: Set(status),
            subtotal: Set(total),
            total: Set(total),
            created_at: Set(placed),
            updated_at: Set(completed.unwrap_or(placed)),
            completed_at: Set(completed),
            ..testing::new_order(self.buyer, shop, &format!("{number:04}"), random_id())
        }
        .insert(&self.db)
        .await
        .unwrap();
        order_item::ActiveModel {
            order_id: Set(order.id),
            product_id: Set(product),
            name: Set("ขนมครก".to_owned()),
            unit: Set("กล่อง".to_owned()),
            unit_price: Set(Money::from_baht(PRICE)),
            quantity: Set(quantity),
            line_total: Set(total),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .unwrap();
    }

    /// The seller's shop and its product
    fn seller(&self) -> (i64, i64) {
        (self.shop, self.product)
    }

    /// Marks `shop` as summarized `on` that day, as a run does as it queues its summary.
    async fn mark(&self, shop: i64, on: NaiveDate) {
        shop::Entity::update_many()
            .col_expr(shop::Column::SummarySentOn, Expr::value(on))
            .filter(shop::Column::Id.eq(shop))
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Sets the user `line_user_id`'s `column` to `value`.
    async fn set_user(&self, line_user_id: &str, column: user::Column, value: Expr) {
        user::Entity::update_many()
            .col_expr(column, value)
            .filter(user::Column::LineUserId.eq(line_user_id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Runs the job's work at `now` with `ai`.
    async fn send(&self, ai: &Gateway, now: DateTime<Utc>) -> u64 {
        let line = self.line.client();
        send(
            &self.db,
            &line,
            ai,
            &self.templates,
            &self.notification,
            now,
        )
        .await
        .unwrap()
    }

    /// A gateway whose model answers with `turns`, and the model.
    fn ai(&self, turns: impl IntoIterator<Item = Turn>) -> (Gateway, Model) {
        scripted_ai::scripted(&self.pool, turns, false, &[])
    }

    /// A gateway with the assistant off, and its model, which is never asked.
    fn no_assistant(&self) -> (Gateway, Model) {
        scripted_ai::scripted(&self.pool, [], false, &[Feature::Assistant])
    }

    /// The pushes LINE got: each one's recipient and messages.
    fn pushes(&self) -> Vec<(String, Value)> {
        self.line
            .requests()
            .iter()
            .filter(|request| request.path == PUSH)
            .map(|request| {
                let body = request.json();
                (
                    body["to"].as_str().unwrap().to_owned(),
                    body["messages"].clone(),
                )
            })
            .collect()
    }

    /// The pushes' recipients, in order.
    fn recipients(&self) -> Vec<String> {
        self.pushes().into_iter().map(|(to, _)| to).collect()
    }

    /// The summary of `day` pushed at `at`, with `comment`, as LINE gets it.
    fn summary(&self, at: DateTime<Utc>, day: &Day, comment: Option<&str>) -> Value {
        let notice: Message = self
            .templates
            .daily_summary_notice(at, day, comment, buttons());
        serde_json::to_value([notice]).unwrap()
    }
}

/// `quantity` กล่อง of the product `product_id` sold for ฿20 each
fn best(product_id: i64, quantity: u64) -> TopProduct {
    TopProduct {
        product_id,
        name: "ขนมครก".to_owned(),
        unit: "กล่อง".to_owned(),
        quantity,
        value: Money::from_baht(PRICE * i64::try_from(quantity).unwrap()),
    }
}

/// The day counts the orders placed on it, whatever their status, and those completed on it,
/// whenever they were placed: a pre-order handed over today counts today (FR-PROD-11). Each
/// seller is told of their own shop's only (NFR-SEC-02), in one push, with buttons to see more.
#[tokio::test]
async fn each_seller_is_pushed_their_shops_day() {
    let s = setup().await;
    let other = s.other_shop(OTHER_SELLER, shop::Status::Approved).await;
    let seller = s.seller();
    // Today: one still waiting, one placed and completed, a pre-order placed two days ago and
    // handed over, and one the seller turned down
    s.order(seller, 1, midnight() + hours(9), None, Pending)
        .await;
    let completed = Some(midnight() + hours(12));
    s.order(seller, 2, midnight() + hours(8), completed, Completed)
        .await;
    let pre_order = midnight() - hours(48);
    s.order(seller, 10, pre_order, completed, Completed).await;
    s.order(seller, 3, midnight() + hours(10), None, Rejected)
        .await;
    // Yesterday, just before midnight: neither placed nor completed today
    let late = Some(midnight() - TimeDelta::seconds(1));
    s.order(seller, 4, midnight() - hours(3), late, Completed)
        .await;
    s.order(seller, 5, midnight() - hours(2), None, Accepted)
        .await;
    // The other seller's, at midnight exactly
    s.order(other, 7, midnight(), Some(midnight()), Completed)
        .await;

    let (ai, model) = s.no_assistant();
    assert_eq!(s.send(&ai, eight_pm()).await, 2);

    let day = Day {
        placed: 3,
        completed: 2,
        completed_value: Money::from_baht(240),
        top: vec![best(s.product, 12)],
    };
    let others = Day {
        placed: 1,
        completed: 1,
        completed_value: Money::from_baht(140),
        top: vec![best(other.1, 7)],
    };
    assert_eq!(
        s.pushes(),
        [
            (SELLER.to_owned(), s.summary(eight_pm(), &day, None)),
            (
                OTHER_SELLER.to_owned(),
                s.summary(eight_pm(), &others, None)
            ),
        ]
    );
    // Logged as what it is, counting against the quota as any push (FR-NOT-05)
    let logged: Vec<(String, push_log::Status)> = push_log::Entity::find()
        .all(&s.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.push_type, row.status))
        .collect();
    let summary = ("seller.daily_summary".to_owned(), push_log::Status::Sent);
    assert_eq!(logged, [summary.clone(), summary]);
    // With the assistant off, the AI isn't asked
    assert!(model.requests().is_empty());
}

/// Acceptance (#47): while `notification.dailySummary` is off, no summary is sent, and the AI
/// isn't asked for a comment either.
#[tokio::test]
async fn no_summary_is_sent_while_the_switch_is_off() {
    let mut s = setup().await;
    let completed = Some(midnight() + hours(12));
    s.order(s.seller(), 2, midnight() + hours(8), completed, Completed)
        .await;
    s.notification.daily_summary = false;
    assert!(!Kind::DailySummary.is_on(&s.notification));

    let (ai, model) = s.ai([Turn::Text("ขายดีมากค่ะ")]);
    assert_eq!(s.send(&ai, eight_pm()).await, 0);
    assert!(s.pushes().is_empty());
    assert!(model.requests().is_empty());
    let shop = shop::Entity::find_by_id(s.shop).one(&s.db).await.unwrap();
    assert_eq!(shop.unwrap().summary_sent_on, None);
}

/// Acceptance (#47), FR-NOT-02: a seller who doesn't follow the OA, or who deleted their
/// account, has no summary, and the AI isn't asked about their shop.
#[tokio::test]
async fn no_summary_is_sent_to_sellers_who_do_not_follow() {
    let s = setup().await;
    let completed = Some(midnight() + hours(12));
    s.order(s.seller(), 2, midnight() + hours(8), completed, Completed)
        .await;
    s.set_user(SELLER, user::Column::IsFollowing, Expr::value(false))
        .await;
    // Deleting the account takes the LINE user ID away (FR-ONB-07)
    let deleted = s.other_shop(OTHER_SELLER, shop::Status::Approved).await;
    s.order(deleted, 1, midnight() + hours(9), None, Pending)
        .await;
    let no_id = Expr::value(Option::<String>::None);
    s.set_user(OTHER_SELLER, user::Column::LineUserId, no_id)
        .await;

    let (ai, model) = s.ai([Turn::Text("ขายดีมากค่ะ"), Turn::Text("ขายดีมากค่ะ")]);
    assert_eq!(s.send(&ai, eight_pm()).await, 0);
    assert!(s.pushes().is_empty());
    assert!(model.requests().is_empty());
}

/// Only the owner of an `APPROVED` or `HELD` shop, who may see the sales its button opens, with
/// an account not suspended, is summarized.
#[tokio::test]
async fn only_sellers_who_may_see_their_sales_are_summarized() {
    let s = setup().await;
    let statuses = [
        ("U-pending", shop::Status::Pending),
        ("U-rejected", shop::Status::Rejected),
        ("U-held", shop::Status::Held),
        ("U-suspended", shop::Status::Suspended),
        ("U-closed", shop::Status::Closed),
    ];
    for (line_user_id, status) in statuses {
        let shop = s.other_shop(line_user_id, status).await;
        s.order(shop, 1, midnight() + hours(9), None, Pending).await;
    }
    // The approved shop's owner is suspended
    s.order(s.seller(), 1, midnight() + hours(9), None, Pending)
        .await;
    let suspended = Expr::value(user::Status::Suspended);
    s.set_user(SELLER, user::Column::Status, suspended).await;

    let (ai, _) = s.no_assistant();
    assert_eq!(s.send(&ai, eight_pm()).await, 1);
    assert_eq!(s.recipients(), ["U-held"]);
}

/// NFR-COST-01: a day with no order placed and no sale completed costs no push and no AI call.
#[tokio::test]
async fn a_quiet_day_sends_nothing() {
    let s = setup().await;
    // Placed yesterday and still open, and completed yesterday
    s.order(s.seller(), 1, midnight() - hours(5), None, Accepted)
        .await;
    let yesterday = Some(midnight() - hours(1));
    s.order(s.seller(), 2, midnight() - hours(6), yesterday, Completed)
        .await;

    let (ai, model) = s.ai([Turn::Text("ขายดีมากค่ะ")]);
    assert_eq!(s.send(&ai, eight_pm()).await, 0);
    assert!(s.pushes().is_empty());
    assert!(model.requests().is_empty());
}

/// With the assistant on, the AI comments on the day as the system, in the bot's voice, given
/// the figures and, as data, the names the seller wrote (FR-AI-06); its comment shows on one
/// line as an aside.
#[tokio::test]
async fn the_ai_comments_on_the_day_when_the_assistant_is_on() {
    let s = setup().await;
    let completed = Some(midnight() + hours(12));
    s.order(s.seller(), 3, midnight() + hours(8), completed, Completed)
        .await;
    let yesterday = Some(midnight() - hours(4));
    s.order(s.seller(), 1, midnight() - hours(6), yesterday, Completed)
        .await;

    let (ai, model) = s.ai([Turn::Text("  วันนี้ขายได้มากกว่าเมื่อวานค่ะ\n\nเก่งมากเลยค่ะ ")]);
    assert_eq!(s.send(&ai, eight_pm()).await, 1);

    let day = Day {
        placed: 1,
        completed: 1,
        completed_value: Money::from_baht(60),
        top: vec![best(s.product, 3)],
    };
    let comment = "วันนี้ขายได้มากกว่าเมื่อวานค่ะ เก่งมากเลยค่ะ";
    assert_eq!(
        s.pushes(),
        [(
            SELLER.to_owned(),
            s.summary(eight_pm(), &day, Some(comment))
        )]
    );

    let requests = model.requests();
    let [request] = requests.as_slice() else {
        panic!("one request: {requests:?}");
    };
    assert!(request.system.starts_with(RULES));
    assert!(request.system.contains("End sentences with ค่ะ."));
    assert!(request.system.ends_with(DATA_RULE));
    let [message] = request.messages.as_slice() else {
        panic!("one message");
    };
    assert_eq!(message.role, Role::User);
    let [Part::Text(data)] = message.parts.as_slice() else {
        panic!("one text");
    };
    let figures = json!({
        "today": {
            "orders_placed": 1,
            "sales_completed": 1,
            "sales_value": "฿60",
            "best_sellers": [
                { "name": "ขนมครก", "quantity": 3, "unit": "กล่อง", "value": "฿60" },
            ],
        },
        "yesterday": { "sales_completed": 1, "sales_value": "฿20" },
    });
    assert_eq!(data, &ai::as_data("sales", &figures));

    // The system's call, which counts against the global limit only (FR-AI-09)
    let logged = ai_call_log::Entity::find().all(&s.db).await.unwrap();
    let logged: Vec<_> = logged
        .iter()
        .map(|row| (row.feature.as_str(), row.user_id))
        .collect();
    assert_eq!(logged, [("assistant", None)]);
}

/// FR-AI-10, NFR-REL-06: without an answer, or with one too long to be a short comment, the
/// summary goes without a comment.
#[tokio::test]
async fn without_a_comment_fit_to_show_the_summary_goes_without_one() {
    let s = setup().await;
    let other = s.other_shop(OTHER_SELLER, shop::Status::Approved).await;
    let completed = Some(midnight() + hours(12));
    for shop in [s.seller(), other] {
        s.order(shop, 2, midnight() + hours(8), completed, Completed)
            .await;
    }

    let rambling: &'static str = "ขายดีมาก".repeat(30).leak();
    assert!(rambling.chars().count() > COMMENT_CHARS);
    let (ai, model) = s.ai([Turn::Fail, Turn::Text(rambling)]);
    assert_eq!(s.send(&ai, eight_pm()).await, 2);
    assert_eq!(model.requests().len(), 2);

    let day = |product| Day {
        placed: 1,
        completed: 1,
        completed_value: Money::from_baht(40),
        top: vec![best(product, 2)],
    };
    assert_eq!(
        s.pushes(),
        [
            (
                SELLER.to_owned(),
                s.summary(eight_pm(), &day(s.product), None)
            ),
            (
                OTHER_SELLER.to_owned(),
                s.summary(eight_pm(), &day(other.1), None)
            ),
        ]
    );
}

/// FR-JOB-03: a run done again the same day, as after one cut short, pushes only the sellers
/// not yet summarized; the next day each seller is summarized again.
#[tokio::test]
async fn a_run_done_again_pushes_no_seller_twice() {
    let s = setup().await;
    let other = s.other_shop(OTHER_SELLER, shop::Status::Approved).await;
    for shop in [s.seller(), other] {
        s.order(shop, 1, midnight() + hours(9), None, Pending).await;
    }
    // A run cut short after the seller's summary was queued; the other's is yesterday's
    let today = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
    s.mark(s.shop, today).await;
    s.mark(other.0, today.pred_opt().unwrap()).await;

    let (ai, _) = s.no_assistant();
    assert_eq!(s.send(&ai, eight_pm()).await, 1);
    assert_eq!(s.recipients(), [OTHER_SELLER]);
    // And run again, later that evening
    assert_eq!(s.send(&ai, eight_pm() + hours(1)).await, 0);
    assert_eq!(s.recipients(), [OTHER_SELLER]);

    // The next day, with an order, the seller is summarized again
    let tomorrow = eight_pm() + hours(24);
    s.order(s.seller(), 1, tomorrow - hours(2), None, Pending)
        .await;
    assert_eq!(s.send(&ai, tomorrow).await, 1);
    assert_eq!(s.recipients(), [OTHER_SELLER, SELLER]);
    let shop = shop::Entity::find_by_id(s.shop).one(&s.db).await.unwrap();
    assert_eq!(shop.unwrap().summary_sent_on, today.succ_opt());
}

/// A run overdue past midnight, as at a restart after the app was down at 20:00, sends nothing:
/// the day before isn't made up for, nor anyone pushed at night. One late the same evening
/// still goes.
#[tokio::test]
async fn a_run_overdue_past_midnight_sends_nothing() {
    let s = setup().await;
    s.order(s.seller(), 1, midnight() + hours(9), None, Pending)
        .await;
    let next_day = midnight() + hours(24);
    s.order(s.seller(), 1, next_day + hours(1), None, Pending)
        .await;

    let (ai, _) = s.no_assistant();
    // 31 ต.ค. 01:30 and 19:59 in Bangkok
    assert_eq!(s.send(&ai, next_day + TimeDelta::minutes(90)).await, 0);
    assert_eq!(s.send(&ai, bangkok(10, 31, 19, 59)).await, 0);
    assert!(s.pushes().is_empty());
    // 31 ต.ค. 23:00: late, but the same day
    assert_eq!(s.send(&ai, bangkok(10, 31, 23, 0)).await, 1);
    assert_eq!(s.recipients(), [SELLER]);
}
