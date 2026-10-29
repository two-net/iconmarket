//! "สถิติ" (FR-ADM-06, FR-NOT-05): the figures against a seeded market, each with a row on
//! either side of its Bangkok midnight; the card that shows them; and who may see it. The
//! `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Iterable, TransactionTrait};
use serde_json::Value;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::line::limits;
use crate::notifications::templates::order_status_name;
use crate::persistence::entity::{counter, order, push_log};
use crate::persistence::{orm, testing};
use crate::thai::Money;
use crate::users::access::{Actor, Denied};

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

/// 22 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    testing::at()
}

fn utc(month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, month, day, hour, minute, second)
        .unwrap()
}

/// 22 ต.ค. 00:00 in Bangkok, when today began
fn today() -> DateTimeUtc {
    utc(10, 21, 17, 0, 0)
}

/// 16 ต.ค. 00:00 in Bangkok, when the last 7 days began
fn week() -> DateTimeUtc {
    utc(10, 15, 17, 0, 0)
}

/// 23 ก.ย. 00:00 in Bangkok, when the last 30 days began
fn month() -> DateTimeUtc {
    utc(9, 22, 17, 0, 0)
}

/// An instant before `at`
fn just_before(at: DateTimeUtc) -> DateTimeUtc {
    at - chrono::TimeDelta::seconds(1)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    config: Config,
    admin: i64,
    buyer: i64,
}

/// An admin and a buyer, and nothing else.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user("U-admin")
    };
    let admin = admin.insert(&db).await.unwrap().id;
    let buyer = user::ActiveModel {
        created_at: Set(just_before(week())),
        ..testing::new_user("U-buyer")
    };
    let buyer = buyer.insert(&db).await.unwrap().id;
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        admin,
        buyer,
    }
}

impl Setup {
    /// User `user_id` taps "สถิติ" at `t0()`, as routing hands it over.
    async fn taps(&self, user_id: i64) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let reply = tapped(&turn, &self.config).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        reply
    }

    async fn gather(&self, config: &Config) -> Statistics {
        gather(&self.db, config, t0()).await.unwrap()
    }

    /// Besides the admin and the buyer: a user new this week, a suspended one, one who
    /// unfollowed, a deleted one, and the owners of shops of every status, one of them open.
    /// Orders placed on either side of each period's start, and the day's AI calls and the
    /// month's pushes. [`market`] is what it holds.
    async fn seed(&self) {
        let db = &self.db;
        let user = |line_user_id: &str, created_at| user::ActiveModel {
            created_at: Set(created_at),
            ..testing::new_user(line_user_id)
        };
        user("U-new", week()).insert(db).await.unwrap();
        user::ActiveModel {
            status: Set(user::Status::Suspended),
            suspension_reason: Set(Some("ขายของต้องห้าม".to_owned())),
            ..user("U-suspended", utc(10, 20, 3, 0, 0))
        }
        .insert(db)
        .await
        .unwrap();
        user::ActiveModel {
            is_following: Set(false),
            ..user("U-unfollowed", utc(9, 1, 3, 0, 0))
        }
        .insert(db)
        .await
        .unwrap();
        user::ActiveModel {
            line_user_id: Set(None),
            status: Set(user::Status::Deleted),
            ..user("U-deleted", t0())
        }
        .insert(db)
        .await
        .unwrap();

        let mut open = None;
        for (i, (status, is_open)) in [
            (shop::Status::Pending, true),
            (shop::Status::Approved, true),
            (shop::Status::Approved, false),
            (shop::Status::Rejected, false),
            (shop::Status::Held, true),
            (shop::Status::Suspended, true),
            (shop::Status::Closed, false),
        ]
        .into_iter()
        .enumerate()
        {
            let owner = user(&format!("U-seller-{i}"), utc(9, 1, 3, 0, 0))
                .insert(db)
                .await
                .unwrap();
            let shop = shop::ActiveModel {
                status: Set(status),
                is_open: Set(is_open),
                deleted_at: Set((status == shop::Status::Closed).then_some(t0())),
                ..testing::new_shop(owner.id)
            }
            .insert(db)
            .await
            .unwrap();
            if status == shop::Status::Approved && is_open {
                open = Some(shop.id);
            }
        }

        let shop = open.expect("an open shop");
        for (n, (placed, status, satang)) in [
            (utc(10, 22, 7, 0, 0), order::Status::Completed, 10_050),
            (utc(10, 22, 3, 0, 0), order::Status::Delivering, 7_000),
            (today(), order::Status::Pending, 5_000),
            (just_before(today()), order::Status::Completed, 20_000),
            (utc(10, 18, 3, 0, 0), order::Status::Cancelled, 8_000),
            (week(), order::Status::Accepted, 6_000),
            (just_before(week()), order::Status::Completed, 30_025),
            (utc(10, 1, 3, 0, 0), order::Status::Expired, 4_000),
            (month(), order::Status::Rejected, 3_000),
            (just_before(month()), order::Status::Completed, 99_900),
        ]
        .into_iter()
        .enumerate()
        {
            let number = format!("IM-20261022-{n:04}");
            let key = Uuid::from_u128(n as u128 + 1);
            order::ActiveModel {
                status: Set(status),
                subtotal: Set(Money::from_satang(satang)),
                total: Set(Money::from_satang(satang)),
                created_at: Set(placed),
                updated_at: Set(placed),
                ..testing::new_order(self.buyer, shop, &number, key)
            }
            .insert(db)
            .await
            .unwrap();
        }

        let admins = format!("ai:20261022:u{}", self.admin);
        for (key, value) in [
            ("ai:20261022", 37),
            // The admin's own, which is part of everyone's
            (admins.as_str(), 9),
            ("ai:20261021", 1_999),
            ("order:20261022", 10),
        ] {
            let counter = counter::ActiveModel {
                key: Set(key.to_owned()),
                value: Set(value),
            };
            counter.insert(db).await.unwrap();
        }

        for (status, sent_at) in [
            (push_log::Status::Sent, utc(10, 22, 3, 0, 0)),
            (push_log::Status::Sent, utc(9, 30, 17, 0, 0)),
            (push_log::Status::Failed, utc(10, 10, 3, 0, 0)),
            // Still to be tried: neither
            (push_log::Status::Pending, utc(10, 22, 7, 0, 0)),
            // 30 ก.ย. 23:59 in Bangkok: last month
            (push_log::Status::Sent, utc(9, 30, 16, 59, 0)),
            (push_log::Status::Failed, utc(9, 30, 16, 59, 0)),
        ] {
            push_log::ActiveModel {
                push_type: Set("order.new".to_owned()),
                recipient: Set("U-seller-1".to_owned()),
                retry_key: Set(None),
                status: Set(status),
                sent_at: Set(sent_at),
                messages: Set(None),
                attempts: Set(1),
                queued_at: Set(sent_at),
                ..Default::default()
            }
            .insert(db)
            .await
            .unwrap();
        }
    }
}

/// What [`Setup::seed`] holds, worked out by hand.
fn market() -> Statistics {
    use order::Status::*;
    let orders = |days, counts: [u64; 8], satang| Orders {
        days,
        by_status: [
            Pending, Accepted, Ready, Delivering, Completed, Rejected, Cancelled, Expired,
        ]
        .into_iter()
        .zip(counts)
        .collect(),
        completed_value: Money::from_satang(satang),
    };
    Statistics {
        // The admin, the buyer, all the others but the deleted one, and the 7 shop owners
        users: 12,
        // The admin, and the users new this week and suspended
        new_users: 3,
        approved_shops: 2,
        open_shops: 1,
        orders: [
            orders(1, [1, 0, 0, 1, 1, 0, 0, 0], 10_050),
            orders(7, [1, 1, 0, 1, 2, 0, 1, 0], 30_050),
            orders(30, [1, 1, 0, 1, 3, 1, 1, 1], 60_075),
        ],
        ai_calls: Some(AiCalls {
            today: 37,
            limit: 2_000,
        }),
        pushes: PushStats { sent: 2, failed: 1 },
    }
}

/// The texts of `message`'s card, in order.
fn texts(message: &Message) -> Vec<String> {
    fn walk(value: &Value, texts: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                if fields.get("type") == Some(&Value::from("text"))
                    && let Some(Value::String(text)) = fields.get("text")
                {
                    texts.push(text.clone());
                }
                fields.values().for_each(|value| walk(value, texts));
            }
            Value::Array(values) => values.iter().for_each(|value| walk(value, texts)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    walk(
        &serde_json::to_value(message).unwrap()["contents"],
        &mut texts,
    );
    texts
}

/// Whether `texts` hold `row` in a row.
fn has_row(texts: &[String], row: &[&str]) -> bool {
    texts.windows(row.len()).any(|window| window == row)
}

/// FR-ADM-06 acceptance: every figure is the database's, counted by the Bangkok day.
#[tokio::test]
async fn the_figures_match_the_database() {
    let s = setup().await;
    let empty = s.gather(&s.config).await;
    let none = |days| Orders {
        days,
        by_status: order::Status::iter().map(|status| (status, 0)).collect(),
        completed_value: Money::ZERO,
    };
    let nothing = Statistics {
        users: 2,
        new_users: 1,
        approved_shops: 0,
        open_shops: 0,
        orders: [none(1), none(7), none(30)],
        ai_calls: Some(AiCalls {
            today: 0,
            limit: 2_000,
        }),
        pushes: PushStats::default(),
    };
    assert_eq!(empty, nothing);

    s.seed().await;
    assert_eq!(s.gather(&s.config).await, market());
}

/// FR-AI-09: with the AI off there are no calls to count; the card says it is off.
#[tokio::test]
async fn the_ai_calls_are_not_counted_while_the_ai_is_off() {
    let mut s = setup().await;
    s.seed().await;
    s.config.ai.primary = None;
    let statistics = s.gather(&s.config).await;
    assert_eq!(
        statistics,
        Statistics {
            ai_calls: None,
            ..market()
        }
    );
    let card = card(&s.templates, &statistics, t0());
    assert!(has_row(&texts(&card), &["ใช้วันนี้", "ปิดอยู่"]));
}

/// "สถิติ" is one card: users, shops, a table of orders by status and period with the value
/// of those completed, the AI calls against the limit and the month's pushes, and a button to
/// see it again.
#[tokio::test]
async fn the_card_shows_the_figures() {
    let s = setup().await;
    s.seed().await;
    let reply = s.taps(s.admin).await;
    limits::check(&reply).unwrap();
    let [card] = &reply[..] else {
        panic!("one card: {reply:?}")
    };
    assert_eq!(card, &super::card(&s.templates, &market(), t0()));
    let texts = texts(card);
    for row in [
        &["สถิติ", "ณ 22 ต.ค. 2569 14:30"][..],
        &["ทั้งหมด", "12 คน"],
        &["ใหม่ใน 7 วัน", "3 คน"],
        &["อนุมัติแล้ว", "2 ร้าน"],
        &["เปิดอยู่ตอนนี้", "1 ร้าน"],
        &["สถานะ", "วันนี้", "7 วัน", "30 วัน"],
        &[order_status_name(order::Status::Pending), "1", "1", "1"],
        &[order_status_name(order::Status::Ready), "0", "0", "0"],
        &[order_status_name(order::Status::Completed), "1", "2", "3"],
        &[order_status_name(order::Status::Expired), "0", "0", "1"],
        &["รวม", "3", "6", "9"],
        &["ยอดที่สำเร็จ", "฿100.50", "฿300.50", "฿600.75"],
        &["ใช้วันนี้", "37 / 2,000 ครั้ง"],
        &["ส่งแล้ว", "2 ครั้ง"],
        &["ส่งไม่สำเร็จ", "1 ครั้ง"],
    ] {
        assert!(has_row(&texts, row), "no {row:?} in {texts:?}");
    }

    let json = serde_json::to_string(card).unwrap();
    assert!(json.contains(r#""data":"stats""#), "{json}");
    assert!(json.contains(labels::REFRESH.as_str()), "{json}");
}

/// FR-ADM-06, NFR-SEC-02: only admins see the statistics, and not while suspended.
#[tokio::test]
async fn only_admins_see_the_statistics() {
    let s = setup().await;
    s.seed().await;
    assert_eq!(
        s.taps(s.buyer).await,
        [s.templates.denied(Denied::NotAllowed)]
    );

    let reason = "ทดสอบการระงับ";
    user::ActiveModel {
        id: Set(s.admin),
        status: Set(user::Status::Suspended),
        suspension_reason: Set(Some(reason.to_owned())),
        ..Default::default()
    }
    .update(&s.db)
    .await
    .unwrap();
    assert_eq!(
        s.taps(s.admin).await,
        [s.templates.refused(Denied::Suspended, Some(reason))]
    );
}
