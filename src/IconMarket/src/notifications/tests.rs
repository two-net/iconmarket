//! Push notifications against a database and a fake LINE: the switches (FR-NOT-01), followers
//! only (FR-NOT-02, AC-17), one request per recipient per step (FR-NOT-04), the push log and its
//! statistics (FR-NOT-05), and retries that never deliver twice (FR-LINE-12). Each test starts a
//! throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use chrono_tz::Asia::Bangkok;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, IntoActiveModel, QueryOrder,
    TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, NotificationConfig};
use crate::line::testing::{Answer, Fake, PUSH};
use crate::line::{Message, RetryKey};
use crate::persistence::entity::push_log::{self, Status};
use crate::persistence::entity::user;
use crate::persistence::{orm, testing};

const SELLER: &str = "U5e11e75e11e75e11e75e11e75e11e7";
const BUYER: &str = "Ub0b0b0b0b0b0b0b0b0b0b0b0b0b0b0";
const ADMIN_1: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const ADMIN_2: &str = "Ua2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";
const ADMIN_3: &str = "Ua3a3a3a3a3a3a3a3a3a3a3a3a3a3a3";

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    Setup {
        _container: container,
        db: orm(pool),
        line: Fake::start().await,
    }
}

/// The sample config's switches, with the daily summary turned on too: all of Table 4.8-A on
fn switches() -> NotificationConfig {
    NotificationConfig {
        daily_summary: true,
        ..config::sample().notification
    }
}

fn minutes(minutes: i64) -> TimeDelta {
    TimeDelta::minutes(minutes)
}

fn text(text: &str) -> Vec<Message> {
    vec![Message::text(text)]
}

impl Setup {
    /// A user with `line_user_id`, an admin or not, following the OA or not.
    async fn user(&self, line_user_id: &str, admin: bool, following: bool) -> i64 {
        let user = user::ActiveModel {
            is_admin: Set(admin),
            is_following: Set(following),
            ..testing::new_user(line_user_id)
        };
        user.insert(&self.db).await.unwrap().id
    }

    /// Queues `outbox` at `now` in a step that commits.
    async fn queue_at(
        &self,
        outbox: Outbox,
        switches: &NotificationConfig,
        now: DateTimeUtc,
    ) -> Queued {
        let txn = self.db.begin().await.unwrap();
        let queued = outbox.queue(&txn, switches, now).await.unwrap();
        txn.commit().await.unwrap();
        queued
    }

    /// Queues `outbox` in a step that commits now, then sends it, as the worker does.
    async fn notify(&self, outbox: Outbox, switches: &NotificationConfig) {
        let queued = self.queue_at(outbox, switches, Utc::now()).await;
        send(&self.db, &self.line.client(), queued).await;
    }

    async fn retry(&self, now: DateTimeUtc) -> Retried {
        retry(&self.db, &self.line.client(), now).await.unwrap()
    }

    /// The push requests LINE got: each one's recipient and message texts.
    fn pushes(&self) -> Vec<(String, Vec<String>)> {
        self.line
            .requests()
            .iter()
            .filter(|request| request.path == PUSH)
            .map(|request| {
                let body = request.json();
                let texts = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|message| message["text"].as_str().unwrap().to_owned())
                    .collect();
                (body["to"].as_str().unwrap().to_owned(), texts)
            })
            .collect()
    }

    async fn logs(&self) -> Vec<push_log::Model> {
        push_log::Entity::find()
            .order_by_asc(push_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }
}

fn push(to: &str, texts: &[&str]) -> (String, Vec<String>) {
    let texts = texts.iter().map(|text| (*text).to_owned()).collect();
    (to.to_owned(), texts)
}

/// AC (#15), FR-NOT-04: two notifications for the same seller in one step are sent as one push
/// request, logged once.
#[tokio::test]
async fn notifications_for_one_recipient_in_a_step_go_in_one_push_request() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let buyer = s.user(BUYER, false, true).await;

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    outbox.add(Kind::OrderStatus, To::User(buyer), text("ร้านรับออเดอร์แล้วค่ะ"));
    outbox.add(Kind::OrderCancelled, To::User(seller), text("ลูกค้ายกเลิกค่ะ"));
    s.notify(outbox, &switches()).await;

    assert_eq!(
        s.pushes(),
        [
            push(SELLER, &["ออเดอร์ใหม่ค่ะ", "ลูกค้ายกเลิกค่ะ"]),
            push(BUYER, &["ร้านรับออเดอร์แล้วค่ะ"]),
        ]
    );
    let logs = s.logs().await;
    assert_eq!(logs.len(), 2);
    let to_seller = &logs[0];
    assert_eq!(to_seller.push_type, "order.new,order.cancelled");
    assert_eq!(to_seller.recipient, SELLER);
    assert_eq!(to_seller.status, Status::Sent);
    assert_eq!((to_seller.attempts, to_seller.error.as_deref()), (1, None));
    // Kept only until sent: it can hold names and addresses
    assert_eq!(to_seller.messages, None);
    let retry_key = to_seller.retry_key.unwrap().to_string();
    assert_eq!(
        s.line.requests()[0].header("x-line-retry-key"),
        Some(retry_key.as_str())
    );
    assert_eq!(logs[1].push_type, "order.status");
    assert_ne!(logs[1].retry_key, to_seller.retry_key);
}

/// AC-17, FR-NOT-02: a seller who blocked the OA gets no push, while the step's other
/// notifications go out.
#[tokio::test]
async fn no_push_is_attempted_to_a_user_who_does_not_follow_the_oa() {
    let s = setup().await;
    let seller = s.user(SELLER, false, false).await;
    let buyer = s.user(BUYER, false, true).await;

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    outbox.add(
        Kind::OrderStatus,
        To::User(buyer),
        text("ส่งออเดอร์ให้ร้านแล้วค่ะ"),
    );
    s.notify(outbox, &switches()).await;

    assert_eq!(s.pushes(), [push(BUYER, &["ส่งออเดอร์ให้ร้านแล้วค่ะ"])]);
    let logs = s.logs().await;
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].recipient, BUYER);

    // Nothing at all for a step that only notifies them, nor for an unknown user
    let mut outbox = Outbox::default();
    outbox.add(Kind::OrderCancelled, To::User(seller), text("ลูกค้ายกเลิกค่ะ"));
    outbox.add(
        Kind::OrderCancelled,
        To::User(seller + 100),
        text("ลูกค้ายกเลิกค่ะ"),
    );
    let queued = s.queue_at(outbox, &switches(), Utc::now()).await;
    assert!(queued.is_empty());
    assert_eq!(s.logs().await.len(), 1);
}

/// AC (#15): turning off `notification.orderStatusToBuyer` stops buyer status pushes.
#[tokio::test]
async fn turning_off_order_status_to_buyer_stops_buyer_status_pushes() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let buyer = s.user(BUYER, false, true).await;
    let switches = NotificationConfig {
        order_status_to_buyer: false,
        ..switches()
    };

    let mut outbox = Outbox::default();
    outbox.add(Kind::OrderStatus, To::User(buyer), text("ร้านรับออเดอร์แล้วค่ะ"));
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    s.notify(outbox, &switches).await;

    assert_eq!(s.pushes(), [push(SELLER, &["ออเดอร์ใหม่ค่ะ"])]);
}

/// The switches of Table 4.8-A all off
fn all_off() -> NotificationConfig {
    NotificationConfig {
        new_order_to_seller: false,
        order_status_to_buyer: false,
        admin_alerts: false,
        daily_summary: false,
        ..switches()
    }
}

/// FR-NOT-01: each switch of Table 4.8-A stops its own notifications; the rest are always sent.
#[tokio::test]
async fn each_switch_stops_its_own_notifications() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let buyer = s.user(BUYER, false, true).await;
    s.user(ADMIN_1, true, true).await;

    // Those with a switch first
    let notifications = [
        (Kind::NewOrder, To::User(seller), "ออเดอร์ใหม่"),
        (Kind::OrderStatus, To::User(buyer), "สถานะออเดอร์"),
        (Kind::ShopApplication, To::Admins, "คำขอเปิดร้าน"),
        (Kind::HeldForReview, To::Admins, "สินค้ารอตรวจ"),
        (Kind::UserReport, To::Admins, "รายงานปัญหา"),
        (Kind::DailySummary, To::User(seller), "สรุปยอดขาย"),
        (Kind::OrderCancelled, To::User(seller), "ยกเลิก"),
        (Kind::OrderExpired, To::User(seller), "หมดเวลา"),
        (Kind::CancellationRequested, To::User(seller), "ขอยกเลิก"),
        (Kind::SlipReceived, To::User(seller), "สลิป"),
        (Kind::ShopApproved, To::User(seller), "อนุมัติร้าน"),
        (Kind::ShopRejected, To::User(seller), "ไม่อนุมัติร้าน"),
        (Kind::ItemHeld, To::User(seller), "สินค้าถูกพัก"),
        (Kind::ItemRemoved, To::User(seller), "สินค้าถูกนำออก"),
        (Kind::AccountSuspended, To::User(buyer), "ระงับบัญชี"),
        (Kind::SlipNotReceived, To::User(buyer), "ยังไม่ได้รับเงิน"),
        (Kind::OrderMessage, To::User(buyer), "ข้อความจากร้าน"),
    ];
    let outbox = |count: usize| {
        let mut outbox = Outbox::default();
        for (kind, to, what) in &notifications[..count] {
            outbox.add(*kind, *to, text(what));
        }
        outbox
    };
    s.notify(outbox(notifications.len()), &all_off()).await;

    assert_eq!(
        s.pushes(),
        [
            push(SELLER, &["ยกเลิก", "หมดเวลา", "ขอยกเลิก", "สลิป", "อนุมัติร้าน"]),
            push(SELLER, &["ไม่อนุมัติร้าน", "สินค้าถูกพัก", "สินค้าถูกนำออก"]),
            push(BUYER, &["ระงับบัญชี", "ยังไม่ได้รับเงิน", "ข้อความจากร้าน"]),
        ]
    );

    let switched: [(fn(&mut NotificationConfig), _); 4] = [
        (
            |c| c.new_order_to_seller = true,
            push(SELLER, &["ออเดอร์ใหม่"]),
        ),
        (
            |c| c.order_status_to_buyer = true,
            push(BUYER, &["สถานะออเดอร์"]),
        ),
        (
            |c| c.admin_alerts = true,
            push(ADMIN_1, &["คำขอเปิดร้าน", "สินค้ารอตรวจ", "รายงานปัญหา"]),
        ),
        (|c| c.daily_summary = true, push(SELLER, &["สรุปยอดขาย"])),
    ];
    for (turn_on, pushed) in switched {
        let mut switches = all_off();
        turn_on(&mut switches);
        let before = s.pushes().len();
        s.notify(outbox(6), &switches).await;
        assert_eq!(s.pushes().split_off(before), [pushed]);
    }
}

/// Admin alerts reach every admin who follows the OA, merged with whatever else the step sends
/// them.
#[tokio::test]
async fn admin_alerts_go_to_every_admin_who_follows_the_oa() {
    let s = setup().await;
    let admin_with_shop = s.user(ADMIN_1, true, true).await;
    s.user(ADMIN_2, true, true).await;
    s.user(ADMIN_3, true, false).await;
    s.user(BUYER, false, true).await;

    let mut outbox = Outbox::default();
    outbox.add(
        Kind::NewOrder,
        To::User(admin_with_shop),
        text("ออเดอร์ใหม่ค่ะ"),
    );
    outbox.add(Kind::ShopApplication, To::Admins, text("มีคำขอเปิดร้านใหม่ค่ะ"));
    s.notify(outbox, &switches()).await;

    assert_eq!(
        s.pushes(),
        [
            push(ADMIN_1, &["ออเดอร์ใหม่ค่ะ", "มีคำขอเปิดร้านใหม่ค่ะ"]),
            push(ADMIN_2, &["มีคำขอเปิดร้านใหม่ค่ะ"]),
        ]
    );
    let types: Vec<_> = s
        .logs()
        .await
        .into_iter()
        .map(|log| log.push_type)
        .collect();
    assert_eq!(
        types,
        ["order.new,admin.shop_application", "admin.shop_application"]
    );
}

/// FR-NOT-04, FR-LINE-11: more than a request holds goes in as few requests as possible, and no
/// notification is split between two.
#[tokio::test]
async fn more_than_five_messages_are_split_between_notifications() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let messages = |prefix: &str, count: usize| -> Vec<Message> {
        (1..=count)
            .map(|i| Message::text(format!("{prefix}{i}")))
            .collect()
    };

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), messages("ก", 3));
    outbox.add(Kind::SlipReceived, To::User(seller), messages("ข", 3));
    outbox.add(Kind::OrderCancelled, To::User(seller), messages("ค", 2));
    s.notify(outbox, &switches()).await;

    assert_eq!(
        s.pushes(),
        [
            push(SELLER, &["ก1", "ก2", "ก3"]),
            push(SELLER, &["ข1", "ข2", "ข3", "ค1", "ค2"]),
        ]
    );
}

/// A notification that breaks LINE's limits is a bug; it is dropped, and the rest still go out.
#[tokio::test]
async fn a_notification_over_a_limit_is_dropped_and_the_rest_are_sent() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;

    let mut outbox = Outbox::default();
    let six = vec![Message::text("ข้อความ"); 6];
    outbox.add(Kind::NewOrder, To::User(seller), six);
    outbox.add(Kind::NewOrder, To::User(seller), text(&"ก".repeat(5001)));
    outbox.add(Kind::OrderCancelled, To::User(seller), text("ลูกค้ายกเลิกค่ะ"));
    s.notify(outbox, &switches()).await;

    assert_eq!(s.pushes(), [push(SELLER, &["ลูกค้ายกเลิกค่ะ"])]);
}

/// A step rolled back notifies no one: its pushes are gone with it.
#[tokio::test]
async fn a_step_rolled_back_notifies_no_one() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    let txn = s.db.begin().await.unwrap();
    let queued = outbox.queue(&txn, &switches(), Utc::now()).await.unwrap();
    assert_eq!(queued.len(), 1);
    txn.rollback().await.unwrap();

    assert!(s.logs().await.is_empty());
    assert_eq!(
        s.retry(Utc::now() + TimeDelta::hours(1)).await,
        Retried::default()
    );
    assert!(s.pushes().is_empty());
}

/// FR-LINE-12: a push that still fails after the client's retries is left to the retry job,
/// which sends it with the same retry key once it is due.
#[tokio::test]
async fn a_failed_push_is_sent_again_by_the_retry_job_with_the_same_key() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    s.line
        .script(PUSH, vec![Answer::error(500, "Internal server error"); 4]);

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    s.notify(outbox, &switches()).await;

    let log = &s.logs().await[0];
    assert_eq!(log.status, Status::Pending);
    assert_eq!(log.attempts, 1);
    assert_eq!(
        log.error.as_deref(),
        Some("LINE answered 500 Internal Server Error: Internal server error [request req-4]")
    );
    assert!(log.messages.is_some());
    assert_eq!(s.line.deliveries(), 0);

    // Not due yet
    let now = Utc::now();
    assert_eq!(s.retry(now).await, Retried::default());
    assert_eq!(s.line.requests().len(), 4);

    let retried = s.retry(now + minutes(6)).await;
    assert_eq!(
        retried,
        Retried {
            sent: 1,
            ..Retried::default()
        }
    );
    assert_eq!(s.line.deliveries(), 1);
    let requests = s.line.requests();
    assert_eq!(requests.len(), 5);
    let key = log.retry_key.unwrap().to_string();
    assert!(
        requests
            .iter()
            .all(|request| request.header("x-line-retry-key") == Some(key.as_str()))
    );
    assert_eq!(requests[4].json(), requests[0].json());
    let log = &s.logs().await[0];
    assert_eq!((log.status, log.attempts), (Status::Sent, 2));
    assert_eq!((log.error.as_deref(), log.messages.as_ref()), (None, None));

    // Done with: later runs send nothing
    assert_eq!(s.retry(now + TimeDelta::hours(2)).await, Retried::default());
    assert_eq!(s.line.requests().len(), 5);
}

/// A push that keeps failing is tried after each wait, then given up.
#[tokio::test]
async fn a_push_that_keeps_failing_is_given_up_after_the_last_wait() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    s.line
        .script(PUSH, vec![Answer::error(503, "Service unavailable"); 20]);

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    s.notify(outbox, &switches()).await;

    let mut at = Utc::now();
    for (attempts, wait) in [(2, 5), (3, 15), (4, 60)] {
        // A minute early, it isn't due
        assert_eq!(s.retry(at + minutes(wait - 1)).await, Retried::default());
        at += minutes(wait);
        let retried = s.retry(at).await;
        assert_eq!(retried.pending, 1, "attempt {attempts}");
        assert_eq!(s.logs().await[0].attempts, attempts);
    }
    at += minutes(180);
    assert_eq!(s.retry(at).await.failed, 1);

    let log = &s.logs().await[0];
    assert_eq!((log.status, log.attempts), (Status::Failed, 5));
    assert!(log.error.as_deref().unwrap().contains("503"));
    assert_eq!(log.messages, None);
    assert_eq!(s.line.requests().len(), 20);
    assert_eq!(s.retry(at + TimeDelta::hours(5)).await, Retried::default());
}

/// FR-LINE-12: the monthly quota is not retried, nor are LINE's other refusals; both count as
/// failures.
#[tokio::test]
async fn the_quota_and_other_refusals_are_given_up_at_once() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let buyer = s.user(BUYER, false, true).await;
    s.line.script(
        PUSH,
        [
            Answer::error(429, "You have reached your monthly limit."),
            Answer::error(400, "Failed to send messages"),
        ],
    );

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    outbox.add(Kind::OrderStatus, To::User(buyer), text("ร้านรับออเดอร์แล้วค่ะ"));
    s.notify(outbox, &switches()).await;

    let logs = s.logs().await;
    let errors: Vec<_> = logs
        .iter()
        .map(|log| log.error.as_deref().unwrap())
        .collect();
    assert_eq!(
        errors,
        [
            "the monthly message quota is used up [request req-1]",
            "LINE answered 400 Bad Request: Failed to send messages [request req-2]",
        ]
    );
    assert!(logs.iter().all(|log| log.status == Status::Failed));
    assert!(logs.iter().all(|log| log.messages.is_none()));

    assert_eq!(
        s.retry(Utc::now() + TimeDelta::hours(1)).await,
        Retried::default()
    );
    assert_eq!(s.line.requests().len(), 2);
}

/// NFR-REL-03: pushes a crash or shutdown cut off after the step committed are sent by the retry
/// job, once, even those LINE delivered before the crash.
#[tokio::test]
async fn pushes_cut_off_by_a_crash_are_sent_by_the_retry_job_once() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let buyer = s.user(BUYER, false, true).await;
    let queued_at = Utc::now() - minutes(2);

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    outbox.add(
        Kind::OrderStatus,
        To::User(buyer),
        text("ส่งออเดอร์ให้ร้านแล้วค่ะ"),
    );
    s.queue_at(outbox, &switches(), queued_at).await;
    // The buyer's push was delivered, but the crash came before its result was recorded
    let to_buyer = s.logs().await.pop().unwrap();
    s.line
        .client()
        .push_stored(
            BUYER,
            to_buyer.messages.as_ref().unwrap(),
            RetryKey::from(to_buyer.retry_key.unwrap()),
        )
        .await
        .unwrap();

    // A push just queued is the step's own to send
    assert_eq!(s.retry(queued_at).await, Retried::default());

    let retried = s.retry(Utc::now()).await;
    assert_eq!(
        retried,
        Retried {
            sent: 2,
            ..Retried::default()
        }
    );
    assert_eq!(s.line.deliveries(), 2);
    assert_eq!(
        s.pushes(),
        [
            push(BUYER, &["ส่งออเดอร์ให้ร้านแล้วค่ะ"]),
            push(SELLER, &["ออเดอร์ใหม่ค่ะ"]),
            push(BUYER, &["ส่งออเดอร์ให้ร้านแล้วค่ะ"]),
        ]
    );
    assert!(
        s.logs()
            .await
            .iter()
            .all(|log| log.status == Status::Sent && log.attempts == 1)
    );
}

/// A push is not sent once too late for its retry key, which LINE forgets after 24 hours, nor to
/// a recipient who stopped following the OA since (FR-NOT-02).
#[tokio::test]
async fn pushes_too_late_or_to_users_who_left_are_given_up_unsent() {
    let s = setup().await;
    let seller = s.user(SELLER, false, true).await;
    let buyer = s.user(BUYER, false, true).await;
    let now = Utc::now();

    let mut outbox = Outbox::default();
    outbox.add(Kind::NewOrder, To::User(seller), text("ออเดอร์ใหม่ค่ะ"));
    s.queue_at(outbox, &switches(), now - TimeDelta::hours(24))
        .await;
    let mut outbox = Outbox::default();
    outbox.add(Kind::OrderStatus, To::User(buyer), text("ร้านรับออเดอร์แล้วค่ะ"));
    s.queue_at(outbox, &switches(), now - minutes(2)).await;
    let mut unfollowed = user::Entity::find_by_id(buyer)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    unfollowed.is_following = Set(false);
    unfollowed.update(&s.db).await.unwrap();

    let retried = s.retry(now).await;
    assert_eq!(
        retried,
        Retried {
            failed: 2,
            ..Retried::default()
        }
    );
    assert!(s.line.requests().is_empty());
    let errors: Vec<_> = s
        .logs()
        .await
        .into_iter()
        .map(|log| (log.status, log.error.unwrap(), log.messages))
        .collect();
    assert_eq!(
        errors,
        [
            (
                Status::Failed,
                "given up: too late to send it only once".to_owned(),
                None
            ),
            (
                Status::Failed,
                "given up: the recipient no longer follows the OA".to_owned(),
                None
            ),
        ]
    );
}

/// FR-NOT-05, FR-ADM-06: the month's pushes sent and failed, the month being Bangkok's.
#[tokio::test]
async fn stats_count_the_months_pushes_in_the_business_time_zone() {
    let s = setup().await;
    let utc = |month, day, hour, minute| {
        Utc.with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .unwrap()
    };
    for (status, sent_at) in [
        // 31 Oct 23:30 in Bangkok: October
        (Status::Sent, utc(10, 31, 16, 30)),
        // 1 Nov 00:30 in Bangkok
        (Status::Sent, utc(10, 31, 17, 30)),
        (Status::Sent, utc(11, 15, 3, 0)),
        (Status::Failed, utc(11, 20, 3, 0)),
        // Still to be tried: neither
        (Status::Pending, utc(11, 20, 4, 0)),
        // 1 Dec 00:00 in Bangkok
        (Status::Sent, utc(11, 30, 17, 0)),
        (Status::Failed, utc(11, 30, 17, 0)),
    ] {
        push_log::ActiveModel {
            push_type: Set("order.new".to_owned()),
            recipient: Set(SELLER.to_owned()),
            retry_key: Set(Some(RetryKey::random().uuid())),
            status: Set(status),
            sent_at: Set(sent_at),
            messages: Set(None),
            attempts: Set(1),
            queued_at: Set(sent_at),
            ..Default::default()
        }
        .insert(&s.db)
        .await
        .unwrap();
    }

    let november = stats(&s.db, utc(11, 20, 12, 0), Bangkok).await.unwrap();
    assert_eq!(november, PushStats { sent: 2, failed: 1 });
    let october = stats(&s.db, utc(10, 31, 16, 59), Bangkok).await.unwrap();
    assert_eq!(october, PushStats { sent: 1, failed: 0 });
    // The same instant is already November in Bangkok
    let november = stats(&s.db, utc(10, 31, 17, 0), Bangkok).await.unwrap();
    assert_eq!(november, PushStats { sent: 2, failed: 1 });
}
