//! The village group digest (FR-NOT-07) against a database and a fake LINE: the products of
//! "ของใหม่วันนี้" (the acceptance of #48), linked to the chat with the OA rather than buttons
//! that do nothing in a group; only while switched on, with something new and the OA in the
//! group; once a day, never overdue from the day before; and nothing without the OA's basic ID.
//! Each test starts a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone};
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use serde_json::{Value, json};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config;
use crate::line::testing::{Answer, BOT_INFO, Fake, PUSH};
use crate::persistence::entity::{product, push_log, shop, user};
use crate::persistence::{orm, testing};
use crate::thai::Money;

const GROUP: &str = "C0123456789abcdef0123456789abcdef";
const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const OTHER_SELLER: &str = "Ue5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";
const BASIC_ID: &str = "@216ruvtu";
/// Where every button of the digest goes
const CHAT: &str = "https://line.me/R/oaMessage/@216ruvtu/";

fn bangkok(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    chrono_tz::Asia::Bangkok
        .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
        .unwrap()
        .with_timezone(&Utc)
}

/// 30 ต.ค. 2569 07:00 in Bangkok: the digest's time, `notification.groupDigestTime`
fn seven_am() -> DateTime<Utc> {
    bangkok(30, 7, 0)
}

fn hours(hours: i64) -> TimeDelta {
    TimeDelta::hours(hours)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
    templates: Templates,
    notification: NotificationConfig,
    base: Url,
    /// The seller's approved, open shop
    shop: i64,
    category: i64,
}

/// A seller with an approved shop that sells nothing yet; the digest switched on.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&db).await.unwrap();
    let category = testing::new_category("ขนม/ของหวาน")
        .insert(&db)
        .await
        .unwrap();
    let config = config::sample();
    let notification = NotificationConfig {
        group_digest: true,
        ..config.notification
    };
    assert_eq!(notification.group_digest_time.to_string(), "07:00:00");
    Setup {
        _container: container,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        notification,
        base: config.app.base_url.clone(),
        shop: shop.id,
        category: category.id,
    }
}

impl Setup {
    /// Product `name` of `shop`, listed at `listed` and now `status`.
    async fn product(
        &self,
        shop: i64,
        name: &str,
        listed: DateTime<Utc>,
        status: product::Status,
    ) -> i64 {
        product::ActiveModel {
            price: Set(Money::from_baht(20)),
            unit: Set("กล่อง".to_owned()),
            status: Set(status),
            created_at: Set(listed),
            updated_at: Set(listed),
            ..testing::new_product(shop, self.category, name, None)
        }
        .insert(&self.db)
        .await
        .unwrap()
        .id
    }

    /// Product `name` of the seller's shop, listed `ago` before the digest's time.
    async fn listed(&self, name: &str, ago: TimeDelta) -> i64 {
        let listed = seven_am() - ago;
        self.product(self.shop, name, listed, product::Status::Active)
            .await
    }

    /// Another seller, `line_user_id`, whose shop is `status` and whose account `suspended` or
    /// not. Returns the shop.
    async fn other_shop(&self, line_user_id: &str, status: shop::Status, suspended: bool) -> i64 {
        let account = if suspended {
            user::Status::Suspended
        } else {
            user::Status::Active
        };
        let seller = user::ActiveModel {
            status: Set(account),
            ..testing::new_user(line_user_id)
        }
        .insert(&self.db)
        .await
        .unwrap();
        shop::ActiveModel {
            name: Set(format!("ร้าน {line_user_id}")),
            status: Set(status),
            ..testing::new_shop(seller.id)
        }
        .insert(&self.db)
        .await
        .unwrap()
        .id
    }

    /// LINE gives the OA's basic ID `times` times.
    fn basic_id(&self, times: usize) {
        let info = json!({ "userId": "U0", "basicId": BASIC_ID, "displayName": "IconMarket" });
        let answers = (0..times).map(|_| Answer::json(200, info.clone()));
        self.line.script(BOT_INFO, answers);
    }

    /// Runs the job's work at `now` for the village group.
    async fn post(&self, now: DateTime<Utc>) -> anyhow::Result<bool> {
        let line = self.line.client();
        post(
            &self.db,
            &line,
            &self.templates,
            &self.notification,
            Some(GROUP),
            &self.base,
            now,
        )
        .await
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

    /// The group's row, if it has one.
    async fn group(&self) -> Option<line_group::Model> {
        line_group::Entity::find_by_id(GROUP)
            .one(&self.db)
            .await
            .unwrap()
    }
}

/// The names on the product cards of a digest's `messages`, in order.
fn names(messages: &Value) -> Vec<String> {
    messages[1]["contents"]["contents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|bubble| bubble.get("hero").is_some())
        .map(|bubble| {
            bubble["body"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Every action in `value`, wherever it is.
fn actions(value: &Value) -> Vec<Value> {
    match value {
        Value::Object(object) => object
            .iter()
            .flat_map(|(key, value)| {
                let own = (key == "action").then(|| value.clone());
                own.into_iter().chain(actions(value))
            })
            .collect(),
        Value::Array(values) => values.iter().flat_map(actions).collect(),
        _ => Vec::new(),
    }
}

/// The acceptance of #48: the digest lists the same products as "ของใหม่วันนี้", in its order,
/// so never a hidden or held product, nor one of a shop buyers don't see. It is one push to the
/// group, logged as what it is, and every tap opens the chat with the OA.
#[tokio::test]
async fn the_group_gets_the_products_of_new_today_at_its_time() {
    let s = setup().await;
    s.listed("ขนมครก", hours(1)).await;
    s.listed("ข้าวต้มมัด", hours(23)).await;
    // Listed long ago, but restocked last night
    let restocked = s.listed("ไข่ไก่", hours(24 * 7)).await;
    product::ActiveModel {
        id: Set(restocked),
        restocked_at: Set(Some(seven_am() - hours(10))),
        updated_at: Set(seven_am() - hours(10)),
        ..Default::default()
    }
    .update(&s.db)
    .await
    .unwrap();
    // Not new: listed a day and a minute ago
    s.listed("กล้วยตาก", hours(24) + TimeDelta::minutes(1)).await;
    // New, but not shown to buyers
    let (listed, shop) = (seven_am() - hours(2), s.shop);
    s.product(shop, "ซ่อนไว้", listed, product::Status::Hidden)
        .await;
    s.product(shop, "รอตรวจ", listed, product::Status::Held)
        .await;
    let held = s.other_shop(OTHER_SELLER, shop::Status::Held, false).await;
    s.product(held, "ร้านรอตรวจ", listed, product::Status::Active)
        .await;
    let suspended = s
        .other_shop(
            "Uf6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6",
            shop::Status::Approved,
            true,
        )
        .await;
    s.product(suspended, "ร้านถูกระงับ", listed, product::Status::Active)
        .await;
    s.basic_id(1);

    assert!(s.post(seven_am()).await.unwrap());

    let listed = listing::page(&s.db, Listing::NewToday, seven_am(), 0)
        .await
        .unwrap();
    let listed: Vec<String> = listed.items.into_iter().map(|(p, _)| p.name).collect();
    assert_eq!(listed, ["ขนมครก", "ไข่ไก่", "ข้าวต้มมัด"]);
    let pushes = s.pushes();
    assert_eq!(pushes.len(), 1);
    let (to, messages) = &pushes[0];
    assert_eq!(to, GROUP);
    assert_eq!(names(messages), listed);
    assert_eq!(
        messages[0]["text"],
        "ของใหม่วันนี้ 30 ต.ค. 2569 มาแล้วค่ะ กดที่สินค้าเพื่อสั่งซื้อในแชท IconMarketค่ะ"
    );
    assert_eq!(messages[1]["altText"], "ของใหม่วันนี้ 30 ต.ค. 2569");

    // Each card's tap and its one button open the chat; no postback, which a group can't use
    let actions = actions(messages);
    assert_eq!(actions.len(), 2 * listed.len());
    for action in &actions {
        assert_eq!(action["type"], "uri", "{action}");
        assert_eq!(action["uri"], CHAT, "{action}");
    }
    assert_eq!(actions[1]["label"], "สั่งซื้อในแชท");

    let logged: Vec<(String, String, push_log::Status)> = push_log::Entity::find()
        .all(&s.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.push_type, row.recipient, row.status))
        .collect();
    let digest = (
        "group.digest".to_owned(),
        GROUP.to_owned(),
        push_log::Status::Sent,
    );
    assert_eq!(logged, [digest]);
    let today = seven_am()
        .with_timezone(&s.templates.time_zone())
        .date_naive();
    assert_eq!(s.group().await.unwrap().digest_sent_on, Some(today));
}

/// FR-SRCH-03: a page of ten, then "ดูเพิ่มเติม", which in the group opens the chat too.
#[tokio::test]
async fn more_than_a_page_leads_to_the_chat() {
    let s = setup().await;
    for i in 0..11 {
        s.listed(&format!("สินค้า {i}"), hours(1) + TimeDelta::minutes(i))
            .await;
    }
    s.basic_id(1);

    assert!(s.post(seven_am()).await.unwrap());

    let (_, messages) = &s.pushes()[0];
    assert_eq!(names(messages).len(), 10);
    let bubbles = messages[1]["contents"]["contents"].as_array().unwrap();
    assert_eq!(bubbles.len(), 11);
    let more = &bubbles[10]["body"]["contents"][0]["action"];
    assert_eq!(
        more,
        &json!({ "type": "uri", "label": "ดูเพิ่มเติม", "uri": CHAT })
    );
}

/// Pushes to a group count once per member (NFR-COST-01): nothing is posted on a day with
/// nothing new, nor while switched off, and LINE isn't even asked for the basic ID.
#[tokio::test]
async fn nothing_is_posted_with_nothing_new_or_while_switched_off() {
    let mut s = setup().await;
    s.listed("กล้วยตาก", hours(30)).await;

    assert!(!s.post(seven_am()).await.unwrap());

    s.listed("ขนมครก", hours(1)).await;
    s.notification.group_digest = false;
    assert!(!s.post(seven_am()).await.unwrap());
    let line = s.line.client();
    let (templates, notification) = (&s.templates, &s.notification);
    let without_group = NotificationConfig {
        group_digest: true,
        ..*notification
    };
    let posted = post(
        &s.db,
        &line,
        templates,
        &without_group,
        None,
        &s.base,
        seven_am(),
    );
    assert!(!posted.await.unwrap());

    assert!(s.line.requests().is_empty());
    assert_eq!(s.group().await, None);
}

/// FR-JOB-03: a run done again posts no second digest that day, and one overdue past midnight
/// posts none; the next day's is posted at its time.
#[tokio::test]
async fn the_digest_is_posted_once_a_day_and_never_overdue() {
    let s = setup().await;
    s.listed("ขนมครก", hours(1)).await;
    s.basic_id(2);

    assert!(s.post(seven_am()).await.unwrap());
    assert!(!s.post(seven_am() + TimeDelta::minutes(5)).await.unwrap());
    // Overdue from the 30th, the app having been down
    assert!(!s.post(bangkok(31, 2, 0)).await.unwrap());
    assert_eq!(s.pushes().len(), 1);

    s.product(
        s.shop,
        "ข้าวต้มมัด",
        bangkok(31, 6, 0),
        product::Status::Active,
    )
    .await;
    assert!(s.post(bangkok(31, 7, 0)).await.unwrap());
    let pushes = s.pushes();
    assert_eq!(pushes.len(), 2);
    // ขนมครก, listed 25 hours before, is new no more
    assert_eq!(names(&pushes[1].1), ["ข้าวต้มมัด"]);
}

/// The digest is posted only while the OA is in the group: not once it has left, and again
/// once it is added back.
#[tokio::test]
async fn nothing_is_posted_while_the_oa_is_out_of_the_group() {
    let s = setup().await;
    s.listed("ขนมครก", hours(1)).await;
    s.basic_id(1);

    left(&s.db, GROUP, seven_am() - hours(12)).await.unwrap();
    assert!(!s.post(seven_am()).await.unwrap());
    assert!(s.pushes().is_empty());

    let (templates, notification) = (&s.templates, &s.notification);
    let at = seven_am() + hours(1);
    let intro = joined(&s.db, templates, notification, Some(GROUP), GROUP, at).await;
    assert_eq!(
        intro.unwrap(),
        [templates.group_joined(notification.group_digest_time)]
    );
    let group = s.group().await.unwrap();
    assert_eq!((group.joined_at, group.left_at), (Some(at), None));

    assert!(s.post(at).await.unwrap());
    assert_eq!(s.pushes().len(), 1);
}

/// Without the OA's basic ID the run fails, posting nothing and marking nothing, so the
/// scheduler tries again shortly.
#[tokio::test]
async fn without_the_basic_id_nothing_is_posted_and_the_run_fails() {
    let s = setup().await;
    s.listed("ขนมครก", hours(1)).await;
    s.line
        .script(BOT_INFO, [Answer::error(500, "Internal server error")]);

    assert!(s.post(seven_am()).await.is_err());
    assert!(s.pushes().is_empty());
    assert_eq!(s.group().await, None);

    s.basic_id(1);
    assert!(s.post(seven_am() + TimeDelta::minutes(1)).await.unwrap());
    assert_eq!(s.pushes().len(), 1);
}

#[test]
fn the_chat_link_holds_the_basic_id() {
    assert_eq!(chat_link(BASIC_ID).as_str(), CHAT);
}
