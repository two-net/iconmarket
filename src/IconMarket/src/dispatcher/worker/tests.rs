//! The worker against a database and a fake LINE: order per user and concurrency across users
//! (FR-LINE-05), events taking effect once and resuming after a restart (NFR-REL-02,
//! NFR-REL-03), the rate limit (NFR-SEC-05) and routing end to end (FR-UI-12). Each test
//! starts a throwaway PostgreSQL, so they need Docker.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryOrder};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::*;
use crate::config::{self, Config};
use crate::dispatcher::menu::Tab;
use crate::line::testing::{Answer, Fake, REPLY};
use crate::notifications::templates;
use crate::persistence::entity::{session, user};
use crate::persistence::{orm, testing};
use crate::users::access::Denied;

const ALICE: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BOB: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const GROUP: &str = "Cc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const ROOM: &str = "Rd4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

/// Retries and looks at the inbox come quickly, so the tests don't wait
const FAST: Pace = Pace {
    retry_after: [Duration::from_millis(10), Duration::from_millis(20)],
    look_every: Duration::from_millis(50),
};

/// The sample config, with the AI off and a rate limit of `per_minute`.
fn config(per_minute: u32) -> Config {
    let mut config = config::sample();
    config.ai.primary = None;
    config.limits.messages_per_user_per_minute = per_minute;
    config
}

struct Setup {
    container: ContainerAsync<Postgres>,
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    templates: Templates,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    Setup {
        container,
        db: orm(pool.clone()),
        pool,
        line: Fake::start().await,
        templates: Templates::new(&config(20).app),
    }
}

/// A worker that is running, and how to stop it.
struct Running {
    tracker: TaskTracker,
    stop: CancellationToken,
    wakeup: Wakeup,
}

impl Running {
    /// Stops it as shutdown does, once it has finished what it has in hand.
    async fn stop(self) {
        self.stop.cancel();
        self.tracker.close();
        self.tracker.wait().await;
    }
}

impl Setup {
    /// Starts a worker on `pool` with a rate limit of `per_minute`, handling up to `sources`
    /// sources at a time.
    fn run_on(&self, pool: &PgPool, per_minute: u32, sources: usize) -> Running {
        let config = config(per_minute);
        let context = Context {
            db: orm(pool.clone()),
            line: self.line.client(),
            ai: ai::Gateway::new(&config.ai, chrono_tz::Asia::Bangkok, pool.clone()),
            templates: Templates::new(&config.app),
            config: Arc::new(config),
        };
        let (tracker, stop, wakeup) = (
            TaskTracker::new(),
            CancellationToken::new(),
            Wakeup::default(),
        );
        Worker::new(
            &tracker,
            stop.clone(),
            context,
            wakeup.clone(),
            sources,
            FAST,
        )
        .start();
        Running {
            tracker,
            stop,
            wakeup,
        }
    }

    fn run(&self) -> Running {
        self.run_on(&self.pool, 20, 2)
    }

    /// The replies LINE got, in the order it got them: each one's token and messages.
    fn replies(&self) -> Vec<(String, Value)> {
        self.line
            .requests()
            .iter()
            .filter(|request| request.path == REPLY)
            .map(|request| {
                let body = request.json();
                let token = body["replyToken"].as_str().unwrap().to_owned();
                (token, body["messages"].clone())
            })
            .collect()
    }

    fn tokens(&self) -> Vec<String> {
        self.replies().into_iter().map(|(token, _)| token).collect()
    }

    /// The main menu of a user who is only a buyer.
    fn menu(&self) -> Message {
        templates::main_menu([Tab::Buyer], false)
    }

    /// The fallback reply (FR-UI-12 step 6), which functions not built yet give too.
    fn fallback(&self) -> Vec<Message> {
        vec![self.templates.fallback(), self.menu()]
    }
}

/// LINE's event `id` of type `kind` from `source`, with its `fields`.
fn event(id: &str, kind: &str, source: Value, fields: Value) -> Value {
    let mut event = json!({
        "type": kind,
        "mode": "active",
        "timestamp": 1_761_300_000_000_u64,
        "webhookEventId": id,
        "deliveryContext": { "isRedelivery": false },
        "replyToken": token(id),
        "source": source,
    });
    for (key, value) in fields.as_object().unwrap() {
        event[key] = value.clone();
    }
    event
}

fn token(id: &str) -> String {
    format!("reply-{id}")
}

fn user(id: &str) -> Value {
    json!({ "type": "user", "userId": id })
}

fn sent(id: &str, from: &str, message: Value) -> Value {
    event(id, "message", user(from), json!({ "message": message }))
}

fn said(id: &str, from: &str, text: &str) -> Value {
    sent(id, from, json!({ "id": id, "type": "text", "text": text }))
}

fn tapped(id: &str, from: &str, data: &str) -> Value {
    event(
        id,
        "postback",
        user(from),
        json!({ "postback": { "data": data } }),
    )
}

/// The inbox key of `payload`'s source, as the webhook stores it.
fn source_key(payload: &Value) -> String {
    let source = &payload["source"];
    ["groupId", "roomId", "userId"]
        .into_iter()
        .find_map(|key| source[key].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Stores `events` in the inbox in order, as just received.
async fn store(db: &DatabaseConnection, events: impl IntoIterator<Item = Value>) {
    let now = Utc::now();
    for payload in events {
        let key = source_key(&payload);
        testing::new_event(&key, payload, now)
            .insert(db)
            .await
            .unwrap();
    }
}

/// Stores `payload` as an event already taken `attempts` times, now `status`.
async fn store_as(db: &DatabaseConnection, payload: Value, status: Status, attempts: i32) {
    let mut row = testing::new_event(&source_key(&payload), payload, Utc::now());
    row.status = Set(status);
    row.attempts = Set(attempts);
    row.insert(db).await.unwrap();
}

async fn inbox(db: &DatabaseConnection) -> Vec<webhook_event::Model> {
    webhook_event::Entity::find()
        .order_by_asc(webhook_event::Column::Id)
        .all(db)
        .await
        .unwrap()
}

/// Waits until `n` events are done with, handled or failed, and returns the inbox.
async fn settled(db: &DatabaseConnection, n: usize) -> Vec<webhook_event::Model> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = inbox(db).await;
        let done = rows
            .iter()
            .filter(|row| matches!(row.status, Status::Done | Status::Failed))
            .count();
        if done >= n {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "{done} of {n} done with: {rows:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until LINE got `n` replies.
async fn replied(setup: &Setup, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while setup.replies().len() < n {
        assert!(Instant::now() < deadline, "only {:?}", setup.tokens());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A user who has consented, so is past onboarding.
async fn onboarded(db: &DatabaseConnection, line_user_id: &str) -> i64 {
    let mut user = testing::new_user(line_user_id);
    user.consent_version = Set(Some(config(20).privacy.notice_version));
    user.insert(db).await.unwrap().id
}

fn statuses(rows: &[webhook_event::Model]) -> Vec<(Status, i32)> {
    rows.iter().map(|row| (row.status, row.attempts)).collect()
}

/// AC: events from one user are always processed in order.
#[tokio::test]
async fn a_users_events_are_handled_in_order() {
    let setup = setup().await;
    let ids: Vec<_> = (1..=10).map(|i| format!("e{i:02}")).collect();
    store(&setup.db, ids.iter().map(|id| said(id, ALICE, "ขอไข่"))).await;

    let running = setup.run();
    let rows = settled(&setup.db, ids.len()).await;
    running.stop().await;

    assert_eq!(statuses(&rows), vec![(Status::Done, 1); ids.len()]);
    let handled: Vec<_> = rows.iter().map(|row| row.processed_at.unwrap()).collect();
    assert!(handled.is_sorted(), "{handled:?}");
    let tokens: Vec<_> = ids.iter().map(|id| token(id)).collect();
    assert_eq!(setup.tokens(), tokens);
}

/// AC: events from different users run concurrently. Alice's first reply hangs; Bob is
/// answered meanwhile, and Alice's second event waits for her first.
#[tokio::test]
async fn users_are_handled_concurrently() {
    let setup = setup().await;
    setup.line.script(REPLY, [Answer::ok().slow()]);
    store(
        &setup.db,
        [said("a1", ALICE, "ขอไข่"), said("a2", ALICE, "ขอไข่")],
    )
    .await;

    let running = setup.run();
    replied(&setup, 1).await;
    store(&setup.db, [said("b1", BOB, "ขอผัก")]).await;
    running.wakeup.notify();
    settled(&setup.db, 3).await;
    running.stop().await;

    assert_eq!(setup.tokens(), [token("a1"), token("b1"), token("a2")]);
}

/// AC: events left unprocessed by a crash are processed after restart. One was cut short at
/// its first attempt, so is tried again; one at its last, so is given up.
#[tokio::test]
async fn events_left_by_a_crash_are_handled_after_a_restart() {
    let setup = setup().await;
    store(&setup.db, [said("a1", ALICE, "ขอไข่")]).await;
    store_as(&setup.db, said("a2", ALICE, "ขอไข่"), Status::Processing, 1).await;
    store(&setup.db, [said("a3", ALICE, "ขอไข่")]).await;
    store_as(
        &setup.db,
        said("b1", BOB, "ขอผัก"),
        Status::Processing,
        MAX_ATTEMPTS,
    )
    .await;

    let running = setup.run();
    let rows = settled(&setup.db, 4).await;
    running.stop().await;

    assert_eq!(
        statuses(&rows),
        [
            (Status::Done, 1),
            (Status::Done, 2),
            (Status::Done, 1),
            (Status::Failed, MAX_ATTEMPTS)
        ]
    );
    assert!(rows[3].error.as_deref().unwrap().contains("cut short"));
    assert!(rows[3].processed_at.is_some());
    assert_eq!(setup.tokens(), [token("a1"), token("a2"), token("a3")]);
}

/// AC: a user over the rate limit gets exactly one slow-down notice; their other events are
/// acknowledged and ignored, and other users are unaffected.
#[tokio::test]
async fn a_user_over_the_rate_limit_gets_exactly_one_notice() {
    let setup = setup().await;
    let ids: Vec<_> = (1..=6).map(|i| format!("a{i}")).collect();
    let mut events: Vec<_> = ids.iter().map(|id| said(id, ALICE, "ขอไข่")).collect();
    events.insert(4, tapped("b1", BOB, "buy"));
    store(&setup.db, events).await;

    let running = setup.run_on(&setup.pool, 3, 2);
    let rows = settled(&setup.db, 7).await;
    running.stop().await;

    assert_eq!(statuses(&rows), vec![(Status::Done, 1); 7]);
    let replies = setup.replies();
    let alice: Vec<_> = replies
        .iter()
        .filter(|(token, _)| token.starts_with("reply-a"))
        .collect();
    let tokens: Vec<_> = alice.iter().map(|(token, _)| token.as_str()).collect();
    assert_eq!(tokens, ["reply-a1", "reply-a2", "reply-a3", "reply-a4"]);
    assert_eq!(alice[0].1, json!(setup.fallback()));
    assert_eq!(alice[3].1, json!([setup.templates.slow_down()]));
    assert!(replies.iter().any(|(token, _)| token == "reply-b1"));
}

/// An attempt that fails changes nothing and is tried again; after the last the user is told
/// something went wrong, once, and their next event is handled.
#[tokio::test]
async fn a_failing_event_is_tried_again_then_given_up() {
    let setup = setup().await;
    // Every attempt fails once the user is registered, on reading their flow
    let rename = |from, to| format!("ALTER TABLE {from} RENAME TO {to}");
    setup
        .db
        .execute_unprepared(&rename("sessions", "sessions_away"))
        .await
        .unwrap();
    store(&setup.db, [said("a1", ALICE, "ขอไข่")]).await;

    let running = setup.run();
    let rows = settled(&setup.db, 1).await;
    assert_eq!(statuses(&rows), [(Status::Failed, MAX_ATTEMPTS)]);
    assert!(
        rows[0].error.as_deref().unwrap().contains("sessions"),
        "{rows:?}"
    );
    assert!(rows[0].processed_at.is_some());
    replied(&setup, 1).await;
    let sorry = Message::text(setup.templates.something_went_wrong());
    assert_eq!(setup.replies(), [(token("a1"), json!([sorry]))]);
    // Registering Alice was undone with the rest
    assert_eq!(user::Entity::find().all(&setup.db).await.unwrap(), []);

    setup
        .db
        .execute_unprepared(&rename("sessions_away", "sessions"))
        .await
        .unwrap();
    store(&setup.db, [said("a2", ALICE, "ขอไข่")]).await;
    running.wakeup.notify();
    let rows = settled(&setup.db, 2).await;
    running.stop().await;
    assert_eq!(statuses(&rows)[1], (Status::Done, 1));
    assert_eq!(setup.tokens(), [token("a1"), token("a2")]);
}

/// NFR-REL-02: an event handled once is never handled again.
#[tokio::test]
async fn a_handled_event_is_not_handled_again() {
    let setup = setup().await;
    store_as(&setup.db, said("a1", ALICE, "ขอไข่"), Status::Done, 1).await;
    store_as(
        &setup.db,
        said("a2", ALICE, "ขอไข่"),
        Status::Failed,
        MAX_ATTEMPTS,
    )
    .await;
    store(&setup.db, [said("a3", ALICE, "ขอไข่")]).await;

    let running = setup.run();
    let rows = settled(&setup.db, 3).await;
    tokio::time::sleep(FAST.look_every * 3).await;
    running.stop().await;

    assert_eq!(
        statuses(&rows),
        [
            (Status::Done, 1),
            (Status::Failed, MAX_ATTEMPTS),
            (Status::Done, 1)
        ]
    );
    assert_eq!(setup.tokens(), [token("a3")]);
}

/// FR-JOB-04: two instances on one database handle each event once, each user's in order. An
/// event they raced for may show an attempt more than was made.
#[tokio::test]
async fn two_instances_handle_each_event_once_in_order() {
    let setup = setup().await;
    let other_pool = testing::pool(&setup.container).await;
    let ids: Vec<_> = (1..=8).map(|i| format!("{i}")).collect();
    let events = ids.iter().flat_map(|id| {
        [
            said(&format!("a{id}"), ALICE, "ขอไข่"),
            said(&format!("b{id}"), BOB, "ขอผัก"),
        ]
    });
    store(&setup.db, events).await;

    let one = setup.run_on(&setup.pool, 20, 1);
    let other = setup.run_on(&other_pool, 20, 1);
    let rows = settled(&setup.db, 16).await;
    one.stop().await;
    other.stop().await;

    for row in &rows {
        assert_eq!(row.status, Status::Done, "{row:?}");
    }
    let tokens = setup.tokens();
    for prefix in ["reply-a", "reply-b"] {
        let theirs: Vec<_> = tokens
            .iter()
            .filter(|token| token.starts_with(prefix))
            .cloned()
            .collect();
        let expected: Vec<_> = ids.iter().map(|id| format!("{prefix}{id}")).collect();
        assert_eq!(theirs, expected);
    }
    assert_eq!(tokens.len(), 16);
}

/// FR-LINE-06: group and multi-person chats, and other event types, are acknowledged and
/// ignored; an unfollow registers no one.
#[tokio::test]
async fn other_chats_and_event_types_are_ignored() {
    let setup = setup().await;
    let group = json!({ "type": "group", "groupId": GROUP, "userId": ALICE });
    let room = json!({ "type": "room", "roomId": ROOM, "userId": ALICE });
    let text = |id: &str| json!({ "message": { "id": id, "type": "text", "text": "เมนู" } });
    store(
        &setup.db,
        [
            event("g1", "message", group.clone(), text("g1")),
            event("g2", "message", group.clone(), text("g2")),
            event(
                "g3",
                "join",
                json!({ "type": "group", "groupId": GROUP }),
                json!({}),
            ),
            event("r1", "message", room, text("r1")),
            event("u1", "unfollow", user(ALICE), json!({})),
            event("u2", "memberJoined", user(ALICE), json!({})),
            event(
                "u3",
                "unsend",
                user(ALICE),
                json!({ "unsend": { "messageId": "1" } }),
            ),
        ],
    )
    .await;

    let running = setup.run();
    let rows = settled(&setup.db, 7).await;
    tokio::time::sleep(FAST.look_every * 3).await;
    running.stop().await;

    assert_eq!(statuses(&rows), vec![(Status::Done, 1); 7]);
    assert_eq!(setup.line.requests().len(), 0);
    assert_eq!(user::Entity::find().all(&setup.db).await.unwrap(), []);
}

/// FR-UI-12, FR-UI-14, FR-UI-09, FR-UI-15: what each input gets, end to end.
#[tokio::test]
async fn input_is_routed_by_its_priority() {
    let setup = setup().await;
    let t = &setup.templates;
    let alice = onboarded(&setup.db, ALICE).await;
    // Alice left a step unanswered too long ago
    let expired = session::ActiveModel {
        user_id: Set(alice),
        flow: Set("add_product".to_owned()),
        step: Set("price".to_owned()),
        data: Set(json!({})),
        expires_at: Set(Utc::now() - TimeDelta::minutes(1)),
    };
    expired.insert(&setup.db).await.unwrap();
    let image = json!({ "id": "1", "type": "image", "contentProvider": { "type": "line" } });
    let sticker = json!({ "id": "2", "type": "sticker", "packageId": "446", "stickerId": "1988" });
    let location = json!({ "id": "3", "type": "location", "latitude": 13.75, "longitude": 100.5 });
    store(
        &setup.db,
        [
            said("a1", ALICE, "เมนู"),
            sent("a2", ALICE, image),
            sent("a3", ALICE, sticker),
            sent("a4", ALICE, location),
            tapped("a5", ALICE, "Browse"),
            tapped("a6", ALICE, "buy"),
            said("a7", ALICE, "ขอไข่ 2 แผง"),
            said("a8", ALICE, "ยกเลิก"),
            event(
                "b1",
                "follow",
                user(BOB),
                json!({ "follow": { "isUnblocked": false } }),
            ),
            said("b2", BOB, "เมนู"),
        ],
    )
    .await;

    let running = setup.run();
    settled(&setup.db, 10).await;
    // Alice's step now belongs to a flow this release doesn't have
    let gone = session::ActiveModel {
        user_id: Set(alice),
        flow: Set("test.gone".to_owned()),
        step: Set("name".to_owned()),
        data: Set(json!({})),
        expires_at: Set(Utc::now() + TimeDelta::minutes(15)),
    };
    gone.insert(&setup.db).await.unwrap();
    store(&setup.db, [said("a9", ALICE, "ไข่ไก่")]).await;
    running.wakeup.notify();
    settled(&setup.db, 11).await;
    running.stop().await;

    let menu = setup.menu();
    let expected = [
        ("a1", vec![t.step_expired(15), menu.clone()]),
        ("a2", vec![t.image_not_expected()]),
        ("a3", vec![t.unsupported(), menu.clone()]),
        ("a4", vec![t.location_not_expected()]),
        ("a5", vec![t.denied(Denied::NotFound)]),
        ("a6", setup.fallback()),
        ("a7", setup.fallback()),
        ("a8", vec![menu.clone()]),
        // Bob hasn't consented yet, so gets onboarding, which is #19's
        ("b1", setup.fallback()),
        ("b2", setup.fallback()),
        ("a9", vec![t.step_expired(15), t.fallback(), menu.clone()]),
    ];
    let mut replies = setup.replies();
    replies.sort_by_key(|(token, _)| {
        expected
            .iter()
            .position(|(id, _)| *token == self::token(id))
            .unwrap()
    });
    for ((id, messages), (token, got)) in expected.iter().zip(&replies) {
        assert_eq!(*token, self::token(id));
        assert_eq!(*got, json!(messages), "{id}");
    }
    assert_eq!(replies.len(), expected.len());
    let bob = user::Entity::find()
        .all(&setup.db)
        .await
        .unwrap()
        .into_iter()
        .any(|user| user.line_user_id.as_deref() == Some(BOB));
    assert!(bob, "registered on following");
}

/// NFR-REL-05: once stopped, the worker takes no more events.
#[tokio::test]
async fn a_stopped_worker_takes_no_more_events() {
    let setup = setup().await;
    let running = setup.run();
    let wakeup = running.wakeup.clone();
    running.stop().await;

    store(&setup.db, [said("a1", ALICE, "ขอไข่")]).await;
    wakeup.notify();
    tokio::time::sleep(FAST.look_every * 3).await;
    assert_eq!(statuses(&inbox(&setup.db).await), [(Status::Pending, 0)]);
    assert_eq!(setup.line.requests().len(), 0);
}
