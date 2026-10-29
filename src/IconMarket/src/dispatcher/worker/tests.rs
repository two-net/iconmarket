//! The worker against a database and a fake LINE: order per user and concurrency across users
//! (FR-LINE-05), events taking effect once and resuming after a restart (NFR-REL-02,
//! NFR-REL-03), the rate limit (NFR-SEC-05) and routing end to end (FR-UI-12). Each test
//! starts a throwaway PostgreSQL, so they need Docker.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, IntoActiveModel, QueryOrder,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::*;
use crate::config::{self, Config};
use crate::dispatcher::Turn;
use crate::dispatcher::menu::{Item, Tab};
use crate::line::testing::{Answer, Fake, PUSH, REPLY};
use crate::line::{Action, QuickReply};
use crate::notifications::templates::{Browsed, labels};
use crate::notifications::{Kind, Outbox, RichMenus, To, templates};
use crate::orders::cart;
use crate::persistence::entity::user::MenuRoles;
use crate::persistence::entity::{
    audit_log, category, image, join_attempt, product, push_log, report, session, shop, user,
};
use crate::persistence::{orm, testing};
use crate::thai::{Label, Money};
use crate::users::access::{Actor, Denied};
use crate::users::help::Topic;
use crate::users::roles::Roles;
use crate::users::{onboarding, profile};

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
    /// What a worker on `pool` with a rate limit of `per_minute` works with.
    fn context(&self, pool: &PgPool, per_minute: u32) -> Context {
        let config = config(per_minute);
        Context {
            db: orm(pool.clone()),
            line: self.line.client(),
            ai: ai::Gateway::new(&config.ai, chrono_tz::Asia::Bangkok, pool.clone()),
            templates: Templates::new(&config.app),
            config: Arc::new(config),
            rich_menus: None,
        }
    }

    /// Starts a worker on `pool` with a rate limit of `per_minute`, handling up to `sources`
    /// sources at a time.
    fn run_on(&self, pool: &PgPool, per_minute: u32, sources: usize) -> Running {
        self.run_with(self.context(pool, per_minute), sources)
    }

    /// Starts a worker with `context`, handling up to `sources` sources at a time.
    fn run_with(&self, context: Context, sources: usize) -> Running {
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

    /// The privacy notice of the config's version (FR-ONB-02); `updated` for a user who
    /// accepted an earlier one.
    fn notice(&self, config: &Config, updated: bool) -> Message {
        onboarding::notice(&self.templates, config, updated)
    }

    /// The users, by their LINE user ID.
    async fn user(&self, line_user_id: &str) -> user::Model {
        user::Entity::find()
            .filter(user::Column::LineUserId.eq(line_user_id))
            .one(&self.db)
            .await
            .unwrap()
            .expect("registered")
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

/// A user who has consented, so is past onboarding, and whose LINE profile is fresh, so isn't
/// fetched (FR-LINE-10).
async fn onboarded(db: &DatabaseConnection, line_user_id: &str) -> i64 {
    let mut user = testing::new_user(line_user_id);
    user.consent_version = Set(Some(config(20).privacy.notice_version));
    user.profile_refreshed_at = Set(Some(Utc::now()));
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
    onboarded(&setup.db, ALICE).await;
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
            tapped("a6", ALICE, "cart"),
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
    let notice = setup.notice(&config(20), false);
    let empty_cart = t.empty_cart(vec![
        Item::Buy
            .postback()
            .button(labels::CONTINUE_SHOPPING.clone()),
        Item::Search.postback().button(labels::SEARCH.clone()),
        Action::message(labels::MENU.clone(), labels::MENU.as_str()),
    ]);
    let expected = [
        ("a1", vec![t.step_expired(15), menu.clone()]),
        ("a2", vec![t.image_not_expected()]),
        ("a3", vec![t.unsupported(), menu.clone()]),
        ("a4", vec![t.location_not_expected()]),
        ("a5", vec![t.denied(Denied::NotFound)]),
        ("a6", vec![empty_cart]),
        ("a7", setup.fallback()),
        ("a8", vec![menu.clone()]),
        // Bob hasn't consented yet, so gets onboarding
        ("b1", vec![t.welcome(None, "หมู่บ้านไอคอน"), notice.clone()]),
        ("b2", vec![notice]),
        ("a9", vec![t.step_expired(15), t.fallback(), menu.clone()]),
    ];
    assert_replies(setup.replies(), &expected);
    let bob = user::Entity::find()
        .all(&setup.db)
        .await
        .unwrap()
        .into_iter()
        .any(|user| user.line_user_id.as_deref() == Some(BOB));
    assert!(bob, "registered on following");
}

/// Sorts `replies` into the order of `expected`, by their tokens, and checks each against it.
fn assert_replies(mut replies: Vec<(String, Value)>, expected: &[(&str, Vec<Message>)]) {
    replies.sort_by_key(|(token, _)| {
        expected
            .iter()
            .position(|(id, _)| *token == self::token(id))
            .unwrap_or_else(|| panic!("unexpected reply {token}"))
    });
    for ((id, messages), (token, got)) in expected.iter().zip(&replies) {
        assert_eq!(*token, self::token(id));
        assert_eq!(*got, json!(messages), "{id}");
    }
    assert_eq!(replies.len(), expected.len());
}

/// AC (FR-ONB-01 – 03, FR-ONB-08): a new follower is welcomed and can use only help and
/// `#myid` until they accept the privacy notice and enter the join code.
#[tokio::test]
async fn a_new_follower_can_use_only_help_and_myid_until_onboarded() {
    let s = setup().await;
    let t = &s.templates;
    let mut context = s.context(&s.pool, 20);
    let config = Arc::get_mut(&mut context.config).unwrap();
    config.village.join_code = Some("ICON2569".to_owned());
    let accept = onboarding::accept(&config.privacy).to_string();
    let (notice, village) = (s.notice(config, false), config.village.name.clone());
    let profile = format!("/v2/bot/profile/{BOB}");
    let picture = "https://profile.line-scdn.net/0hbob";
    s.line.script(
        &profile,
        [Answer::json(
            200,
            json!({ "userId": BOB, "displayName": "บ๊อบ", "pictureUrl": picture }),
        )],
    );
    store(
        &s.db,
        [
            event(
                "b1",
                "follow",
                user(BOB),
                json!({ "follow": { "isUnblocked": false } }),
            ),
            said("b2", BOB, "เมนู"),
            tapped("b3", BOB, "buy"),
            said("b4", BOB, "#myid"),
            said("b5", BOB, "ช่วยเหลือ"),
            tapped("b6", BOB, &accept),
            said("b7", BOB, "เมนู"),
            said("b8", BOB, "icon๒๕๖๙"),
            said("b9", BOB, "เมนู"),
        ],
    )
    .await;

    let running = s.run_with(context, 2);
    settled(&s.db, 9).await;
    running.stop().await;

    let ask = t.ask_join_code(&village, None);
    let expected = [
        ("b1", vec![t.welcome(Some("บ๊อบ"), &village), notice.clone()]),
        ("b2", vec![notice.clone()]),
        ("b3", vec![notice]),
        ("b4", t.my_id(BOB)),
        // Help works before onboarding
        ("b5", vec![t.help(Topic::for_roles(Roles::default()))]),
        ("b6", vec![ask.clone()]),
        // A keyword is no attempt at the code
        ("b7", vec![ask]),
        ("b8", vec![t.ready(), s.menu()]),
        ("b9", vec![s.menu()]),
    ];
    assert_replies(s.replies(), &expected);

    let bob = s.user(BOB).await;
    assert_eq!(bob.display_name.as_deref(), Some("บ๊อบ"));
    assert_eq!(bob.picture_url.as_deref(), Some(picture));
    assert!(bob.profile_refreshed_at.is_some());
    assert_eq!(bob.consent_version.as_deref(), Some("1"));
    assert!(bob.consented_at.is_some() && bob.joined_at.is_some());
    assert!(bob.is_following);
    let attempts = join_attempt::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(attempts, []);
    // Fetched on following, then not again that day (FR-LINE-10)
    let fetched = s
        .line
        .requests()
        .iter()
        .filter(|r| r.path == profile)
        .count();
    assert_eq!(fetched, 1);
}

/// AC (NFR-PRIV-01): a new `privacy.noticeVersion` asks users who accepted the old one again
/// at their next message, and a tap on the old notice accepts nothing.
#[tokio::test]
async fn bumping_the_notice_version_asks_again() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let old = onboarding::accept(&config(20).privacy).to_string();
    let mut context = s.context(&s.pool, 20);
    let config = Arc::get_mut(&mut context.config).unwrap();
    config.privacy.notice_version = "2".to_owned();
    let new = onboarding::accept(&config.privacy).to_string();
    let notice = s.notice(config, true);
    store(
        &s.db,
        [
            said("a1", ALICE, "เมนู"),
            tapped("a2", ALICE, &old),
            tapped("a3", ALICE, &new),
            said("a4", ALICE, "เมนู"),
            tapped("a5", ALICE, &new),
        ],
    )
    .await;

    let running = s.run_with(context, 2);
    settled(&s.db, 5).await;
    running.stop().await;

    let expected = [
        ("a1", vec![notice.clone()]),
        ("a2", vec![notice]),
        ("a3", vec![t.ready(), s.menu()]),
        ("a4", vec![s.menu()]),
        // A repeated tap has no further effect
        ("a5", vec![s.menu()]),
    ];
    assert_replies(s.replies(), &expected);
    let user = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.consent_version.as_deref(), Some("2"));
}

/// AC (FR-ONB-04, FR-ONB-05): an onboarded user sees "ข้อมูลของฉัน", corrects their phone, an
/// invalid number being refused with guidance, and shares their location; each time the card
/// shows what is now held. A user not yet onboarded gets the privacy notice instead.
#[tokio::test]
async fn my_data_is_shown_and_corrected_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let shared = json!({
        "id": "a6",
        "type": "location",
        "latitude": 13.75,
        "longitude": 100.5,
        "address": "หมู่บ้านไอคอน",
    });
    store(
        &s.db,
        [
            tapped("a1", ALICE, "my_data"),
            tapped("a2", ALICE, "my_data.edit?part=phone"),
            said("a3", ALICE, "12345"),
            said("a4", ALICE, "081-234-5678"),
            tapped("a5", ALICE, "my_data.edit?part=location"),
            sent("a6", ALICE, shared),
            tapped("b1", BOB, "my_data"),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 7).await;
    running.stop().await;

    let config = config(20);
    let user = s.user(ALICE).await;
    assert_eq!(user.phone.as_deref(), Some("0812345678"));
    assert_eq!((user.latitude, user.longitude), (Some(13.75), Some(100.5)));
    // "ข้อมูลของฉัน" as it is now, which the last reply ends with
    let txn = s.db.begin().await.unwrap();
    let actor = Actor::load(&txn, alice).await.unwrap().unwrap();
    let turn = Turn::new(&txn, t, &config.session, actor, Utc::now())
        .await
        .unwrap();
    let card = profile::show(&turn, &config, &turn.actor.user)
        .await
        .unwrap();
    assert_eq!(turn.flow, None, "the flow is over");
    drop(turn);
    txn.rollback().await.unwrap();

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    let says = |label: &Label| Action::message(label.clone(), label.as_str());
    let prompt = |ask: Message, choices: Vec<Action>| {
        let ask = ask.with_quick_reply(QuickReply::new(choices));
        templates::with_cancel(ask)
    };
    let skip = says(&labels::SKIP);
    let share = Action::Location {
        label: labels::SHARE_LOCATION.clone(),
    };
    // The card, before the phone was given
    let before = reply("a1");
    assert_eq!(before[0]["altText"], "ข้อมูลของฉัน");
    assert!(!before.to_string().contains("081-234-5678"));
    assert_eq!(
        reply("a2"),
        json!([prompt(t.ask_phone(None), vec![skip.clone()])])
    );
    assert_eq!(
        reply("a3"),
        json!([prompt(t.phone_invalid(false), vec![skip.clone()])])
    );
    let saved = reply("a4");
    assert_eq!(saved[0], json!(t.phone_saved(Some("081-234-5678"))));
    assert_eq!(saved[1]["altText"], "ข้อมูลของฉัน");
    assert!(saved[1].to_string().contains("เบอร์โทร"));
    assert!(saved[1].to_string().contains("081-234-5678"));
    assert_eq!(
        reply("a5"),
        json!([prompt(t.ask_location(false), vec![share, skip])])
    );
    assert_eq!(
        reply("a6"),
        json!([t.location_saved(true), card[0].clone()])
    );
    // Bob hasn't consented yet
    assert_eq!(reply("b1"), json!([s.notice(&config, false)]));
    assert_eq!(replies.len(), 7);
}

/// AC (FR-ADM-05, FR-ADM-08, FR-SRCH-01): an admin moves a category up and deactivates another,
/// and a buyer's "ซื้อของ" then offers the categories in the new order, without the
/// deactivated one; deleting a category with products is refused. Each change is audited.
#[tokio::test]
async fn categories_set_by_an_admin_are_what_buyers_browse() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut admin = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    admin.is_admin = Set(true);
    admin.menu_roles = Set(Some(MenuRoles::Admin));
    admin.update(&s.db).await.unwrap();
    onboarded(&s.db, BOB).await;
    let mut added = Vec::new();
    for (name, sort_order) in [("อาหาร", 1), ("ขนม/ของหวาน", 2), ("ไข่/นม", 3)]
    {
        let category = category::ActiveModel {
            sort_order: Set(sort_order),
            ..testing::new_category(name)
        };
        added.push(category.insert(&s.db).await.unwrap());
    }
    let [food, sweets, eggs] = added.try_into().unwrap();
    let seller = testing::new_user("U-seller").insert(&s.db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&s.db).await.unwrap();
    testing::new_product(shop.id, food.id, "ข้าวมันไก่", None)
        .insert(&s.db)
        .await
        .unwrap();

    store(
        &s.db,
        [
            tapped("a1", ALICE, "categories"),
            tapped(
                "a2",
                ALICE,
                &format!("categories.up?c={}&n={}", sweets.id, food.id),
            ),
            tapped("a3", ALICE, &format!("categories.deactivate?c={}", eggs.id)),
            tapped("a4", ALICE, &format!("categories.delete?c={}", food.id)),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 4).await;
    // Bob browses once the admin is done
    store(&s.db, [tapped("b1", BOB, "buy")]).await;
    running.wakeup.notify();
    settled(&s.db, 5).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    // The titles of the cards of "หมวดหมู่", the `at`th message of a reply
    let titles = |id: &str, at: usize| -> Vec<String> {
        let cards = reply(id)[at]["contents"]["contents"].clone();
        let cards = cards.as_array().cloned().unwrap_or_default();
        cards
            .iter()
            .map(|card| {
                card["header"]["contents"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(titles("a1", 0), ["1. อาหาร", "2. ขนม/ของหวาน", "3. ไข่/นม"]);
    assert_eq!(titles("a2", 0), ["1. ขนม/ของหวาน", "2. อาหาร", "3. ไข่/นม"]);
    assert_eq!(reply("a3")[0], json!(t.category_deactivated("ไข่/นม")));
    let postback = |label: Label, data: String| Action::Postback {
        display_text: Some(label.as_str().to_owned()),
        label,
        data,
    };
    let deactivate = postback(
        labels::DEACTIVATE.clone(),
        format!("categories.deactivate?c={}", food.id),
    );
    assert_eq!(
        reply("a4"),
        json!([t.category_in_use("อาหาร", Some(deactivate))])
    );
    let choice = |category: &category::Model| {
        let data = format!("browse?category={}", category.id);
        postback(Label::fit(category.name.as_str()), data)
    };
    let new_today = postback(labels::NEW_TODAY.clone(), "browse.new".to_owned());
    let all_shops = postback(labels::ALL_SHOPS.clone(), "browse.shops".to_owned());
    assert_eq!(
        reply("b1"),
        json!([t.choose_category(vec![new_today, all_shops, choice(&sweets), choice(&food)])])
    );
    assert_eq!(replies.len(), 5);

    let exists = category::Entity::find_by_id(food.id).one(&s.db).await;
    assert!(exists.unwrap().is_some(), "not deleted");
    let audited: Vec<_> = audit_log::Entity::find()
        .order_by_asc(audit_log::Column::Id)
        .all(&s.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.actor_id, row.action, row.target_id))
        .collect();
    assert_eq!(
        audited,
        [
            (Some(alice), "category.moved".to_owned(), Some(sweets.id)),
            (
                Some(alice),
                "category.deactivated".to_owned(),
                Some(eggs.id)
            ),
        ]
    );
}

/// Acceptance (#28), FR-SRCH-03, FR-SRCH-04: a buyer taps "ค้นหา", types part of a name and gets
/// what contains it, ten at a time; "ดูเพิ่มเติม" pages through the rest, and the search ends
/// once answered, so what they type next is routed as usual.
#[tokio::test]
async fn a_buyer_searches_and_pages_through_what_is_found() {
    let s = setup().await;
    let t = &s.templates;
    onboarded(&s.db, ALICE).await;
    let seller = testing::new_user("U-seller").insert(&s.db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&s.db).await.unwrap();
    let sweets = testing::new_category("ขนม/ของหวาน")
        .insert(&s.db)
        .await
        .unwrap();
    for number in 1..=11 {
        let name = format!("ขนมครก {number}");
        testing::new_product(shop.id, sweets.id, &name, Some(5))
            .insert(&s.db)
            .await
            .unwrap();
    }
    let hidden = product::ActiveModel {
        status: Set(product::Status::Hidden),
        ..testing::new_product(shop.id, sweets.id, "ขนมครกซ่อน", Some(5))
    };
    hidden.insert(&s.db).await.unwrap();

    store(
        &s.db,
        [tapped("a1", ALICE, "search"), said("a2", ALICE, "ครก")],
    )
    .await;
    let running = s.run();
    settled(&s.db, 2).await;
    replied(&s, 2).await;
    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let found = replies[&token("a2")][0].clone();
    let cards = found["contents"]["contents"].as_array().unwrap().clone();
    let more = cards.last().unwrap()["body"]["contents"][0]["action"]["data"]
        .as_str()
        .unwrap()
        .to_owned();
    store(
        &s.db,
        [tapped("a3", ALICE, &more), said("a4", ALICE, "ครก")],
    )
    .await;
    running.wakeup.notify();
    settled(&s.db, 4).await;
    replied(&s, 4).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    assert_eq!(
        replies[&token("a1")],
        json!([templates::with_cancel(t.ask_keyword())])
    );
    assert_eq!(found["altText"], "ผลการค้นหา \"ครก\"");
    assert_eq!(cards.len(), 11, "ten cards and ดูเพิ่มเติม");
    assert!(more.starts_with("browse.search?pa="), "{more}");
    let rest = replies[&token("a3")][0]["contents"]["contents"].clone();
    let rest: Vec<_> = rest
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["body"]["contents"][0]["text"].clone())
        .collect();
    assert_eq!(rest, [json!("ขนมครก 1")]);
    // The search ended once answered
    assert_eq!(replies[&token("a4")], json!(s.fallback()));
}

/// AC (FR-HELP-01, FR-HELP-02): "ช่วยเหลือ" offers each user their roles' topics, and a buyer's
/// report on a product, once its reason is given, is filed and pushed to the admins after the
/// reply.
#[tokio::test]
async fn help_and_a_report_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let bob = onboarded(&s.db, BOB).await;
    let mut admin = user::Entity::find_by_id(bob)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    admin.is_admin = Set(true);
    admin.menu_roles = Set(Some(MenuRoles::Admin));
    admin.update(&s.db).await.unwrap();
    let seller = testing::new_user("U-seller").insert(&s.db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let product = testing::new_product(shop.id, eggs.id, "ไข่ไก่เบอร์ 2", Some(8));
    let product = product.insert(&s.db).await.unwrap();

    store(
        &s.db,
        [
            said("a1", ALICE, "ช่วยเหลือ"),
            tapped("a2", ALICE, "help.report"),
            tapped("a3", ALICE, &format!("report?p={}", product.id)),
            said("a4", ALICE, "ไข่เน่า ทั้งแผง"),
            tapped("b1", BOB, "help"),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 5).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !s.line.requests().iter().any(|request| request.path == PUSH) {
        assert!(Instant::now() < deadline, "the admins were never alerted");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    let admin = Roles {
        seller: false,
        admin: true,
    };
    assert_eq!(
        reply("a1"),
        json!([t.help(Topic::for_roles(Roles::default()))])
    );
    assert!(
        reply("a2")[0]["text"]
            .as_str()
            .unwrap()
            .contains("กด \"แจ้งปัญหา\" ที่สินค้าหรือร้านนั้น")
    );
    let asked = reply("a3")[0]["text"].as_str().unwrap().to_owned();
    assert!(asked.contains("สินค้า \"ไข่ไก่เบอร์ 2\" ของร้านป้าแดง"), "{asked}");
    let what = templates::Reported::Product {
        name: "ไข่ไก่เบอร์ 2",
        shop: "ร้านป้าแดง",
    };
    assert_eq!(reply("a4"), json!([t.report_received(&what), s.menu()]));
    assert_eq!(reply("b1"), json!([t.help(Topic::for_roles(admin))]));
    assert_eq!(replies.len(), 5);

    let reports = report::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (
            reports[0].reporter_id,
            reports[0].target_id,
            reports[0].reason.as_str()
        ),
        (Some(alice), product.id, "ไข่เน่า ทั้งแผง")
    );
    // Pushed to the admin, once the reporter was answered
    let sent: Vec<_> = s
        .line
        .requests()
        .into_iter()
        .map(|request| {
            let body = request.json();
            let to = body["to"].as_str().or(body["replyToken"].as_str());
            (request.path, to.map(str::to_owned))
        })
        .collect();
    let at = |wanted: &(String, Option<String>)| sent.iter().position(|got| got == wanted);
    let answered = at(&(REPLY.to_owned(), Some(token("a4")))).unwrap();
    let alerted = at(&(PUSH.to_owned(), Some(BOB.to_owned()))).unwrap();
    assert!(answered < alerted, "{sent:?}");
    assert_eq!(sent.iter().filter(|(path, _)| path == PUSH).count(), 1);
    let logs = push_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(
        logs.iter()
            .map(|log| (log.push_type.as_str(), log.status))
            .collect::<Vec<_>>(),
        [("admin.user_report", push_log::Status::Sent)]
    );
}

/// Acceptance (#31): an admin approves a held product from "รายการรอตรวจ", which a buyer then
/// finds; then finds that buyer in "ผู้ใช้" by name and suspends them with a reason, which their
/// next attempt to order is refused with.
#[tokio::test]
async fn the_review_queue_and_a_suspension_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut named = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    named.display_name = Set(Some("อลิซ".to_owned()));
    named.update(&s.db).await.unwrap();
    let bob = onboarded(&s.db, BOB).await;
    let mut admin = user::Entity::find_by_id(bob)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    admin.is_admin = Set(true);
    admin.menu_roles = Set(Some(MenuRoles::Admin));
    admin.update(&s.db).await.unwrap();
    let seller = testing::new_user("U-seller").insert(&s.db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let held = product::ActiveModel {
        status: Set(product::Status::Held),
        ..testing::new_product(shop.id, eggs.id, "ไข่ไก่เบอร์ 2", Some(8))
    };
    let held = held.insert(&s.db).await.unwrap();
    let filed = report::ActiveModel {
        reporter_id: Set(None),
        target_type: Set(report::TargetType::Product),
        target_id: Set(held.id),
        reason: Set("มีคำต้องห้าม \"กัญชา\"".to_owned()),
        status: Set(report::Status::Open),
        created_at: Set(testing::at()),
        ..Default::default()
    };
    let filed = filed.insert(&s.db).await.unwrap();
    let approve = format!(
        "review.approve?p={}&v={}&r={}",
        held.id,
        held.updated_at.timestamp_micros(),
        filed.id
    );

    store(
        &s.db,
        [
            tapped("b1", BOB, "review_queue"),
            tapped("b2", BOB, &approve),
            tapped("b3", BOB, "users"),
            said("b4", BOB, "อลิซ"),
            tapped("b5", BOB, &format!("accounts.suspend?u={alice}")),
            said("b6", BOB, "โกงเงินมัดจำ"),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 6).await;
    // Alice comes once the admin is done
    store(
        &s.db,
        [
            tapped("a1", ALICE, &format!("browse.product?product={}", held.id)),
            tapped("a2", ALICE, &format!("cart.add?product={}", held.id)),
        ],
    )
    .await;
    running.wakeup.notify();
    settled(&s.db, 8).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    assert_eq!(reply("b1")[0]["altText"], "รายการรอตรวจ");
    let what = templates::Reported::Product {
        name: "ไข่ไก่เบอร์ 2",
        shop: "ร้านป้าแดง",
    };
    assert_eq!(reply("b2"), json!([t.review_approved(&what)]));
    assert_eq!(reply("b4")[0]["altText"], "ผู้ใช้");
    assert_eq!(reply("b6"), json!([t.user_suspended(Some("อลิซ"))]));
    assert_eq!(reply("a1")[0]["altText"], "ไข่ไก่เบอร์ 2");
    let refused = t.refused(Denied::Suspended, Some("โกงเงินมัดจำ"));
    assert_eq!(reply("a2"), json!([refused]));

    let product = product::Entity::find_by_id(held.id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(product.status, product::Status::Active);
    let actions: Vec<_> = audit_log::Entity::find()
        .order_by_asc(audit_log::Column::Id)
        .all(&s.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.actor_id, row.action))
        .collect();
    assert_eq!(
        actions,
        [
            (Some(bob), "product.approved".to_owned()),
            (Some(bob), "user.suspended".to_owned()),
        ]
    );
    let logs = push_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(
        logs.iter()
            .map(|log| (log.push_type.as_str(), log.recipient.as_str()))
            .collect::<Vec<_>>(),
        [("user.suspended", ALICE)]
    );
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

/// FR-NOT-04, FR-LINE-07: what an event notifies is pushed once it is answered, while its reply
/// token is fresh, and each push is logged (FR-NOT-05).
#[tokio::test]
async fn an_events_notifications_are_pushed_after_its_reply() {
    let s = setup().await;
    let seller = testing::new_user(BOB).insert(&s.db).await.unwrap();
    let context = s.context(&s.pool, 20);
    let mut outbox = Outbox::default();
    outbox.add(
        Kind::NewOrder,
        To::User(seller.id),
        vec![Message::text("ออเดอร์ใหม่ค่ะ")],
    );
    let txn = s.db.begin().await.unwrap();
    let pushes = outbox
        .queue(&txn, &context.config.notification, Utc::now())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let worker = Worker::new(
        &TaskTracker::new(),
        CancellationToken::new(),
        context,
        Wakeup::default(),
        1,
        FAST,
    );

    let reply = Reply {
        token: Some(token("e1")),
        to: ALICE.to_owned(),
        messages: vec![Message::text("สั่งซื้อแล้วค่ะ")],
        essential: false,
    };
    worker
        .send(super::Answer {
            handled: Handled {
                reply: Some(reply),
                pushes,
                relink: Vec::new(),
                unlink: Vec::new(),
            },
            received_at: Utc::now(),
            span: Span::none(),
        })
        .await;

    let requests: Vec<_> = s
        .line
        .requests()
        .into_iter()
        .map(|request| {
            let to = request.json()["to"].as_str().map(str::to_owned);
            (request.path, to)
        })
        .collect();
    assert_eq!(
        requests,
        [
            (REPLY.to_owned(), None),
            (PUSH.to_owned(), Some(BOB.to_owned())),
        ]
    );
    let logs = push_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(
        (logs[0].status, logs[0].push_type.as_str()),
        (push_log::Status::Sent, "order.new")
    );
}

/// FR-UI-10: a user whose rich menu doesn't match their roles, like an admin listed before their
/// first message, is linked the right one once their event is answered, and only once.
#[tokio::test]
async fn an_outdated_menu_is_relinked_after_the_reply() {
    let s = setup().await;
    let alice = onboarded(&s.db, ALICE).await;
    let mut admin = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    admin.is_admin = Set(true);
    admin.update(&s.db).await.unwrap();
    store(&s.db, [said("e1", ALICE, "เมนู"), said("e2", ALICE, "เมนู")]).await;

    let mut context = s.context(&s.pool, 20);
    context.rich_menus = Some(RichMenus::for_test(vec![
        (MenuRoles::Seller, "richmenu-seller".to_owned()),
        (MenuRoles::Admin, "richmenu-admin".to_owned()),
        (MenuRoles::SellerAdmin, "richmenu-both".to_owned()),
    ]));
    let running = s.run_with(context, 2);
    settled(&s.db, 2).await;
    running.stop().await;

    let paths: Vec<_> = s.line.requests().into_iter().map(|r| r.path).collect();
    let link = format!("/v2/bot/user/{ALICE}/richmenu/richmenu-admin");
    assert_eq!(paths, [REPLY, link.as_str(), REPLY]);
    let user = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.menu_roles, Some(MenuRoles::Admin));
}

/// FR-ONB-06: an unfollow, unanswered, makes the user no follower, so nothing is pushed to them;
/// following again makes them one, and greets them back.
#[tokio::test]
async fn unfollowing_stops_pushes_until_following_again() {
    let s = setup().await;
    let alice = onboarded(&s.db, ALICE).await;
    let notified = async || {
        let mut outbox = Outbox::default();
        let notice = vec![Message::text("ออเดอร์ใหม่ค่ะ")];
        outbox.add(Kind::NewOrder, To::User(alice), notice);
        let (config, txn) = (config(20), s.db.begin().await.unwrap());
        let queued = outbox.queue(&txn, &config.notification, Utc::now());
        let queued = queued.await.unwrap().len();
        txn.rollback().await.unwrap();
        queued
    };
    assert_eq!(notified().await, 1);

    let running = s.run();
    store(&s.db, [event("u1", "unfollow", user(ALICE), json!({}))]).await;
    running.wakeup.notify();
    settled(&s.db, 1).await;
    assert!(!s.user(ALICE).await.is_following);
    assert_eq!(s.line.requests().len(), 0);
    assert_eq!(notified().await, 0);

    let follow = json!({ "follow": { "isUnblocked": true } });
    store(&s.db, [event("f1", "follow", user(ALICE), follow)]).await;
    running.wakeup.notify();
    settled(&s.db, 2).await;
    replied(&s, 1).await;
    running.stop().await;
    assert!(s.user(ALICE).await.is_following);
    let village = config(20).village.name;
    let back = vec![s.templates.welcome_back(None, &village), s.menu()];
    assert_replies(s.replies(), &[("f1", back)]);
    assert_eq!(notified().await, 1);
}

/// AC-13, FR-ONB-07 end to end: a seller taps "ลบบัญชี", confirms and is answered; their account
/// is anonymized, the events they sent before go, and their seller's menu is unlinked once they
/// are answered. Their LINE account, writing again, is someone new, asked to consent.
#[tokio::test]
async fn an_account_is_deleted_end_to_end() {
    let s = setup().await;
    let alice = onboarded(&s.db, ALICE).await;
    user::Entity::update_many()
        .col_expr(user::Column::MenuRoles, Expr::value(MenuRoles::Seller))
        .filter(user::Column::Id.eq(alice))
        .exec(&s.db)
        .await
        .unwrap();
    let shop = testing::new_shop(alice).insert(&s.db).await.unwrap().id;
    let running = s.run();
    // Not by the inbox, as deleting the account deletes the events handled before
    let answer = async |event: Value, n: usize| {
        store(&s.db, [event]).await;
        running.wakeup.notify();
        replied(&s, n).await;
        s.replies()[n - 1].1.clone()
    };

    let ask = answer(tapped("d1", ALICE, crate::users::DELETE_ACCOUNT), 1).await;
    let confirm = offered(&ask, "ยืนยันลบ");
    let deleted = answer(pressed("d2", ALICE, &confirm), 2).await;
    let unlink = format!("/v2/bot/user/{ALICE}/richmenu");
    requested(&s, |path, _| path == unlink).await;
    let again = answer(said("d3", ALICE, "เมนู"), 3).await;
    running.stop().await;

    assert_eq!(deleted, json!([s.templates.account_deleted()]));
    assert_eq!(again, json!([s.notice(&config(20), false)]));
    let requests: Vec<_> = s
        .line
        .requests()
        .into_iter()
        .map(|request| (request.method.to_string(), request.path))
        .collect();
    let profile = format!("/v2/bot/profile/{ALICE}");
    let expected = [
        ("POST", REPLY),
        ("POST", REPLY),
        ("DELETE", unlink.as_str()),
        ("GET", profile.as_str()),
        ("POST", REPLY),
    ];
    let expected = expected.map(|(method, path)| (method.to_owned(), path.to_owned()));
    assert_eq!(requests, expected);

    let old = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (old.status, old.line_user_id, old.consent_version),
        (user::Status::Deleted, None, None)
    );
    let new = s.user(ALICE).await;
    assert_ne!(new.id, alice);
    assert_eq!(
        (new.status, new.consent_version),
        (user::Status::Active, None)
    );
    let shop = shop::Entity::find_by_id(shop).one(&s.db).await.unwrap();
    assert_eq!(shop.unwrap().status, shop::Status::Closed);
    let events: Vec<_> = inbox(&s.db)
        .await
        .into_iter()
        .map(|row| (row.event_id, row.status))
        .collect();
    let handled = |id: &str| (id.to_owned(), Status::Done);
    assert_eq!(events, [handled("d2"), handled("d3")]);
}

/// The pushes LINE got, in order: each one's recipient and messages.
fn pushes(setup: &Setup) -> Vec<(String, Value)> {
    setup
        .line
        .requests()
        .iter()
        .filter(|request| request.path == PUSH)
        .map(|request| {
            let body = request.json();
            let to = body["to"].as_str().unwrap().to_owned();
            (to, body["messages"].clone())
        })
        .collect()
}

/// The JSON body of `request`; `Null` for one without, such as a rich menu's link.
fn body(request: &crate::line::testing::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or(Value::Null)
}

/// Waits until LINE got a request that is `wanted`, by its path and body.
async fn requested(setup: &Setup, wanted: impl Fn(&str, &Value) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let requests = setup.line.requests();
        if requests
            .iter()
            .any(|request| wanted(&request.path, &body(request)))
        {
            return;
        }
        assert!(Instant::now() < deadline, "never requested");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Acceptance (Appendix A.5, FR-SHOP-01 – FR-SHOP-04): a villager applies from the menu, the
/// admin is pushed the application with its buttons and approves it with one tap, and the new
/// seller is pushed the news, with "เพิ่มสินค้า", and linked the seller's menu.
#[tokio::test]
async fn a_shop_is_applied_for_and_approved_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut applicant = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    applicant.zone = Set(Some("ซอย 3".to_owned()));
    applicant.house_number = Set(Some("45/2".to_owned()));
    applicant.update(&s.db).await.unwrap();
    let bob = onboarded(&s.db, BOB).await;
    let mut admin = user::Entity::find_by_id(bob)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    admin.is_admin = Set(true);
    admin.menu_roles = Set(Some(MenuRoles::Admin));
    admin.update(&s.db).await.unwrap();

    store(
        &s.db,
        [
            tapped("a1", ALICE, "my_shop"),
            said("a2", ALICE, "ร้านป้าแดง"),
            said("a3", ALICE, "ไข่ไก่ ผักสวนครัว"),
            said("a4", ALICE, "ข้าม"),
            said("a5", ALICE, "ใช้ที่อยู่บ้าน"),
            said("a6", ALICE, "ข้าม"),
            said("a7", ALICE, "081-234-5678"),
            said("a8", ALICE, "ได้ทั้งสองแบบ"),
            said("a9", ALICE, "10"),
            said("a10", ALICE, "ไม่มีขั้นต่ำ"),
            said("a11", ALICE, "ข้าม"),
            tapped("a12", ALICE, "open_shop.submit?r=10"),
        ],
    )
    .await;
    let mut context = s.context(&s.pool, 20);
    context.rich_menus = Some(RichMenus::for_test(vec![
        (MenuRoles::Seller, "richmenu-seller".to_owned()),
        (MenuRoles::Admin, "richmenu-admin".to_owned()),
        (MenuRoles::SellerAdmin, "richmenu-both".to_owned()),
    ]));
    let running = s.run_with(context, 2);
    settled(&s.db, 12).await;
    requested(&s, |path, body| path == PUSH && body["to"] == BOB).await;

    // The admin's push: the application's card, with its buttons
    let [(to, alert)] = &pushes(&s)[..] else {
        panic!("one push: {:?}", pushes(&s));
    };
    assert_eq!(to, BOB);
    assert_eq!(alert[0]["altText"], "มีคำขอเปิดร้านใหม่ค่ะ");
    let card = serde_json::to_string(&alert[0]["contents"]).unwrap();
    for shown in [
        "ร้านป้าแดง",
        "ไข่ไก่ ผักสวนครัว",
        "ซอย 3 บ้านเลขที่ 45/2",
        "รับเองหรือให้ร้านไปส่ง ค่าส่ง ฿10",
    ] {
        assert!(card.contains(shown), "{shown} in {card}");
    }
    let buttons = alert[0]["contents"]["footer"]["contents"]
        .as_array()
        .unwrap();
    let labels: Vec<_> = buttons
        .iter()
        .map(|button| button["action"]["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["อนุมัติ", "ไม่อนุมัติ"]);
    let approve = buttons[0]["action"]["data"].as_str().unwrap().to_owned();

    store(&s.db, [tapped("b1", BOB, &approve)]).await;
    running.wakeup.notify();
    settled(&s.db, 13).await;
    let link = format!("/v2/bot/user/{ALICE}/richmenu/richmenu-seller");
    requested(&s, |path, _| path == link).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    assert_eq!(
        reply("a12"),
        json!([
            t.application_sent("ร้านป้าแดง"),
            templates::main_menu([Tab::Buyer], true)
        ])
    );
    assert_eq!(reply("b1"), json!([t.shop_approved("ร้านป้าแดง")]));
    assert_eq!(reply("b1")[0]["text"], "อนุมัติร้านป้าแดงแล้วค่ะ");
    assert_eq!(replies.len(), 13);

    let add_product = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    let approved = t.shop_approved_notice("ร้านป้าแดง", add_product);
    assert_eq!(pushes(&s)[1], (ALICE.to_owned(), json!([approved])));
    let shop = shop::Entity::find()
        .filter(shop::Column::OwnerId.eq(alice))
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shop.status, shop::Status::Approved);
    let audited: Vec<_> = audit_log::Entity::find()
        .all(&s.db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.actor_id, row.action, row.target_id))
        .collect();
    assert_eq!(
        audited,
        [(Some(bob), "shop.approved".to_owned(), Some(shop.id))]
    );
    // Answered, then told, then given the seller's menu
    let paths: Vec<_> = s
        .line
        .requests()
        .into_iter()
        .map(|request| {
            let body = body(&request);
            let to = body["to"].as_str().or(body["replyToken"].as_str());
            (request.path, to.map(str::to_owned))
        })
        .collect();
    let at = |wanted: (&str, Option<&str>)| {
        paths
            .iter()
            .position(|(path, to)| path == wanted.0 && to.as_deref() == wanted.1)
            .unwrap()
    };
    let answered = at((REPLY, Some(token("b1").as_str())));
    let told = at((PUSH, Some(ALICE)));
    let linked = at((link.as_str(), None));
    assert!(answered < told && told < linked, "{paths:?}");
    let alice = s.user(ALICE).await;
    assert_eq!(alice.menu_roles, Some(MenuRoles::Seller));
}

/// FR-SHOP-05 – FR-SHOP-09, end to end: the seller closes the shop from the menu, sets its
/// opening hours in "ตั้งค่าร้าน", and sees it as buyers do, closed, with "ดูสินค้า".
#[tokio::test]
async fn a_shop_is_run_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut seller = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    seller.menu_roles = Set(Some(MenuRoles::Seller));
    seller.update(&s.db).await.unwrap();
    let shop = shop::ActiveModel {
        phone: Set(Some("0812345678".to_owned())),
        pickup_address: Set(Some("ซอย 3 บ้านเลขที่ 45/2".to_owned())),
        ..testing::new_shop(alice)
    };
    let shop = shop.insert(&s.db).await.unwrap();
    let products = format!("browse.shop?shop={}", shop.id);

    store(
        &s.db,
        [
            tapped("a1", ALICE, "open_close"),
            tapped("a2", ALICE, "shop_settings"),
            tapped("a3", ALICE, "shop.part?p=opening_hours"),
            said("a4", ALICE, "ทุกวัน 06:00–10:00"),
            tapped("a5", ALICE, "shop.front"),
            tapped("a6", ALICE, &products),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 6).await;
    replied(&s, 6).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    let closed = reply("a1");
    assert!(
        closed[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("ปิดร้านป้าแดงแล้วค่ะ"),
        "{closed}"
    );
    let switch_back = &closed[0]["quickReply"]["items"][0]["action"];
    assert_eq!(switch_back["data"], "shop.open?to=1");
    assert_eq!(reply("a2").as_array().unwrap().len(), 2);
    assert_eq!(reply("a4")[0], json!(t.shop_settings_saved()));
    let front = serde_json::to_string(&reply("a5")[1]).unwrap();
    for shown in ["ร้านปิด", "ทุกวัน 06:00–10:00", "ดูสินค้า", &products]
    {
        assert!(front.contains(shown), "{shown} in {front}");
    }
    assert!(
        !front.contains("081-234-5678") && !front.contains("ซอย 3"),
        "{front}"
    );
    // Its products, of which it has none yet (FR-SRCH-08)
    let none = t.nothing_found(Browsed::Shop("ร้านป้าแดง"), Vec::new());
    assert_eq!(reply("a6")[0]["text"], json!(none)["text"]);
    assert_eq!(replies.len(), 6);

    let shop = shop::Entity::find_by_id(shop.id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert!(!shop.is_open);
    assert_eq!(shop.opening_hours.as_deref(), Some("ทุกวัน 06:00–10:00"));
}

/// Appendix A.3, AC-15, NFR-USE-02, end to end, with the AI on: a seller's photo, one short
/// message and one tap list ขนมครก at ฿20 a กล่อง, 30 of them, in ขนม/ของหวาน, and nothing is
/// listed before the tap. Then a few words restock another product once confirmed (FR-PROD-10).
#[tokio::test]
async fn a_product_is_listed_from_a_photo_and_a_message_end_to_end() {
    use crate::ai::Feature;
    use crate::ai::testing::{self as scripted, Turn as Scripted};

    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut seller = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    seller.menu_roles = Set(Some(MenuRoles::Seller));
    seller.update(&s.db).await.unwrap();
    let shop = testing::new_shop(alice).insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let sweets = testing::new_category("ขนม/ของหวาน")
        .insert(&s.db)
        .await
        .unwrap();
    let sold_out = testing::new_product(shop.id, eggs.id, "ไข่ไก่", Some(0))
        .insert(&s.db)
        .await
        .unwrap();
    let jpeg = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");
    s.line.script(
        "/v2/bot/message/p1/content",
        [Answer::bytes("image/jpeg", jpeg)],
    );
    let draft = json!({
        "name": "ขนมครก",
        "price": "20",
        "unit": "กล่อง",
        "category": "ขนม/ของหวาน",
        "stock": "30",
        "description": "ทำสดวันนี้",
    });
    let allow = json!({ "verdict": "allow", "reason": "" });
    let restock = json!({ "intent": "update", "product": sold_out.id.to_string(), "stock": "12" });
    let mut context = s.context(&s.pool, 20);
    context.ai = scripted::scripted(
        &s.pool,
        [
            Scripted::Json(draft),
            Scripted::Json(allow),
            Scripted::Json(restock),
        ],
        true,
        &[Feature::Assistant],
    )
    .0;
    let photo = json!({ "id": "p1", "type": "image", "contentProvider": { "type": "line" } });

    store(
        &s.db,
        [
            sent("a1", ALICE, photo),
            said("a2", ALICE, "ขนมครก กล่องละ 20 วันนี้ทำ 30 กล่อง"),
        ],
    )
    .await;
    let running = s.run_with(context, 2);
    settled(&s.db, 2).await;
    replied(&s, 2).await;
    let products = || async {
        product::Entity::find()
            .order_by_asc(product::Column::Id)
            .all(&s.db)
            .await
            .unwrap()
    };
    // Drafted, and nothing a buyer could see
    assert_eq!(products().await, std::slice::from_ref(&sold_out));

    store(
        &s.db,
        [
            // The one tap: the draft is the first answer since the photo
            tapped("a3", ALICE, "add_product.submit?r=1"),
            said("a4", ALICE, "ไข่ไก่ มีแล้ว 12 แผง"),
        ],
    )
    .await;
    running.wakeup.notify();
    settled(&s.db, 4).await;
    replied(&s, 4).await;
    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    let offered = reply("a1")[0]["text"].as_str().unwrap().to_owned();
    assert!(
        offered.starts_with("ได้รับรูปแล้วค่ะ จะลงขายใช่ไหมคะ พิมพ์ชื่อ ราคา และจำนวนมาได้เลย"),
        "{offered}"
    );
    let preview = serde_json::to_string(&reply("a2")).unwrap();
    for shown in [
        "ร่างสินค้าให้แล้วค่ะ ตรวจดูก่อนนะคะ",
        "฿20 / กล่อง",
        "30 กล่อง",
        "ขนม/ของหวาน",
        "ยืนยันลงขาย",
        "add_product.submit?r=1",
    ] {
        assert!(preview.contains(shown), "{shown} in {preview}");
    }
    let next = vec![
        Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone()),
        Item::AddProduct
            .postback()
            .button(labels::ADD_ANOTHER.clone()),
    ];
    assert_eq!(reply("a3"), json!([t.product_listed("ขนมครก", next)]));
    let listed = products().await;
    assert_eq!(listed.len(), 2);
    assert_eq!(
        (
            listed[1].name.as_str(),
            listed[1].price,
            listed[1].stock,
            listed[1].category_id,
            listed[1].status,
        ),
        (
            "ขนมครก",
            Money::from_baht(20),
            Some(30),
            sweets.id,
            product::Status::Active,
        )
    );

    // A quick update, asked to be confirmed, and confirmed
    let asked = reply("a4");
    let text = asked[0]["text"].as_str().unwrap();
    assert!(
        text.contains("จำนวน: 0 แผง (ลูกค้าจะเห็นว่าหมด) → 12 แผง"),
        "{text}"
    );
    assert_eq!(products().await[0].stock, Some(0));
    let confirm = asked[0]["quickReply"]["items"][0]["action"]["data"]
        .as_str()
        .unwrap()
        .to_owned();
    store(&s.db, [tapped("a5", ALICE, &confirm)]).await;
    running.wakeup.notify();
    settled(&s.db, 5).await;
    replied(&s, 5).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let saved = replies[&token("a5")][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(saved.starts_with("เติมของแล้วค่ะ"), "{saved}");
    assert_eq!(products().await[0].stock, Some(12));
}

/// FR-PROD-01, FR-UI-14, end to end: a seller's photo that no flow takes starts "เพิ่มสินค้า"
/// with it, the answers lead to the preview, and "ยืนยันลงขาย" lists the product with its photo;
/// "เพิ่มสินค้า" from the menu then starts another.
#[tokio::test]
async fn a_product_is_listed_from_a_photo_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut seller = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    seller.menu_roles = Set(Some(MenuRoles::Seller));
    seller.update(&s.db).await.unwrap();
    let shop = testing::new_shop(alice).insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let jpeg = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");
    s.line.script(
        "/v2/bot/message/p1/content",
        [Answer::bytes("image/jpeg", jpeg)],
    );
    let photo = json!({ "id": "p1", "type": "image", "contentProvider": { "type": "line" } });

    store(
        &s.db,
        [
            sent("a1", ALICE, photo),
            said("a2", ALICE, "ถัดไป"),
            said("a3", ALICE, "ไข่ไก่เบอร์ 2"),
            said("a4", ALICE, "110"),
            said("a5", ALICE, "แผง"),
            said("a6", ALICE, "ไข่/นม"),
            said("a7", ALICE, "30"),
            said("a8", ALICE, "ข้าม"),
            // Seven answers since the photo
            tapped("a9", ALICE, "add_product.submit?r=7"),
            tapped("a10", ALICE, "add_product"),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 10).await;
    replied(&s, 10).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    let offered = reply("a1")[0]["text"].as_str().unwrap().to_owned();
    assert!(
        offered.starts_with("ได้รับรูปแล้วค่ะ จะลงขายสินค้านี้ใช่ไหมคะ"),
        "{offered}"
    );
    let preview = serde_json::to_string(&reply("a8")).unwrap();
    for shown in [
        "ยืนยันลงขาย",
        "฿110 / แผง",
        "30 แผง",
        "add_product.submit?r=7",
    ] {
        assert!(preview.contains(shown), "{shown} in {preview}");
    }
    let next = vec![
        Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone()),
        Item::AddProduct
            .postback()
            .button(labels::ADD_ANOTHER.clone()),
    ];
    assert_eq!(reply("a9"), json!([t.product_listed("ไข่ไก่เบอร์ 2", next)]));
    let another = reply("a10")[0]["text"].as_str().unwrap().to_owned();
    assert!(another.starts_with("มาลงสินค้ากันค่ะ"), "{another}");

    let product = product::Entity::find().one(&s.db).await.unwrap().unwrap();
    assert_eq!(
        (
            product.shop_id,
            product.category_id,
            product.price,
            product.stock,
            product.status
        ),
        (
            shop.id,
            eggs.id,
            crate::thai::Money::from_baht(110),
            Some(30),
            product::Status::Active
        )
    );
    let photo = image::Entity::find().one(&s.db).await.unwrap().unwrap();
    assert_eq!(
        (photo.owner_type, photo.owner_id, photo.position),
        (image::OwnerType::Product, product.id, 0)
    );
}

/// FR-PROD-05, FR-PROD-06, end to end: "สินค้าของฉัน" from the menu shows the sold-out product
/// first, saying "หมด"; its "เติมของ" and the quantity make it orderable again.
#[tokio::test]
async fn a_sold_out_product_is_restocked_end_to_end() {
    let s = setup().await;
    let alice = onboarded(&s.db, ALICE).await;
    let mut seller = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    seller.menu_roles = Set(Some(MenuRoles::Seller));
    seller.update(&s.db).await.unwrap();
    let shop = testing::new_shop(alice).insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let listed = testing::new_product(shop.id, eggs.id, "ขนมครก", Some(5));
    listed.insert(&s.db).await.unwrap();
    let sold_out = testing::new_product(shop.id, eggs.id, "ไข่ไก่เบอร์ 2", Some(0));
    let sold_out = sold_out.insert(&s.db).await.unwrap();
    let restock = format!("my_products.part?id={}&p=stock", sold_out.id);

    store(
        &s.db,
        [
            tapped("a1", ALICE, "my_products"),
            tapped("a2", ALICE, &restock),
            said("a3", ALICE, "30"),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 3).await;
    replied(&s, 3).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    let list = serde_json::to_string(&reply("a1")).unwrap();
    for shown in ["หมด", "เติมของ", restock.as_str()] {
        assert!(list.contains(shown), "{shown} in {list}");
    }
    let at = |name: &str| list.find(name).expect(name);
    assert!(at("ไข่ไก่เบอร์ 2") < at("ขนมครก"), "sold out first: {list}");
    let asked = reply("a2")[1]["text"].as_str().unwrap().to_owned();
    assert!(asked.starts_with("ตอนนี้มีขายกี่แผงคะ"), "{asked}");
    assert_eq!(reply("a3")[0]["text"], "เติมของแล้วค่ะ ลูกค้าสั่งสินค้านี้ได้อีกครั้งค่ะ");

    let restocked = product::Entity::find_by_id(sold_out.id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (restocked.stock, restocked.status),
        (Some(30), product::Status::Active)
    );
    let session = session::Entity::find_by_id(alice).one(&s.db).await.unwrap();
    assert_eq!(session, None);
}

/// FR-CART-01 – FR-CART-03, Appendix A.2, end to end: "ใส่ตะกร้า" asks how many, the quantity
/// tapped goes in the cart, and "ดูตะกร้า" or typing "ตะกร้า" shows it, by shop; "สั่งซื้อเลย"
/// checks it out, first asking a new buyer for their address (FR-ONB-04).
#[tokio::test]
async fn a_buyer_puts_a_product_in_the_cart_end_to_end() {
    let s = setup().await;
    let alice = onboarded(&s.db, ALICE).await;
    let seller = testing::new_user("U-seller").insert(&s.db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let product = testing::new_product(shop.id, eggs.id, "ไข่ไก่เบอร์ 2", Some(8));
    let product = product.insert(&s.db).await.unwrap();

    store(
        &s.db,
        [
            tapped("a1", ALICE, &format!("cart.add?product={}", product.id)),
            said("a2", ALICE, "2"),
        ],
    )
    .await;
    let running = s.run();
    settled(&s.db, 2).await;
    replied(&s, 2).await;
    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let added = replies[&token("a2")][0].clone();
    let data = |i: usize| {
        added["quickReply"]["items"][i]["action"]["data"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (order_now, view_cart) = (data(0), data(1));
    store(
        &s.db,
        [
            tapped("a3", ALICE, &view_cart),
            said("a4", ALICE, "ตะกร้า"),
            tapped("a5", ALICE, &order_now),
        ],
    )
    .await;
    running.wakeup.notify();
    settled(&s.db, 5).await;
    replied(&s, 5).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let reply = |id: &str| replies[&token(id)].clone();
    assert_eq!(reply("a1")[0]["text"], "\"ไข่ไก่เบอร์ 2\" เอากี่แผงคะ");
    assert_eq!(added["text"], "ใส่ตะกร้าแล้วค่ะ ไข่ไก่เบอร์ 2 × 2 แผง = ฿220");
    assert_eq!(order_now, format!("checkout?shop={}", shop.id));
    let cart = reply("a3")[0].clone();
    assert_eq!(cart["altText"], "ตะกร้า");
    let shown = serde_json::to_string(&cart).unwrap();
    for part in ["ร้านป้าแดง", "2 แผง × ฿110", "฿220", "สั่งซื้อร้านนี้"]
    {
        assert!(shown.contains(part), "{part} in {shown}");
    }
    assert_eq!(reply("a4"), reply("a3"));
    let t = &s.templates;
    let ask_address = templates::with_cancel(t.ask_zone(true, false, None));
    assert_eq!(reply("a5"), json!([ask_address]));

    let lines = cart::lines(&s.db, alice).await.unwrap();
    let lines: Vec<_> = lines
        .iter()
        .map(|line| (line.product.id, line.item.quantity))
        .collect();
    assert_eq!(lines, [(product.id, 2)]);
    let session = session::Entity::find_by_id(alice).one(&s.db).await.unwrap();
    assert_eq!(
        session.map(|session| session.flow).as_deref(),
        Some("address")
    );
}

/// The action labelled `label` among the buttons and Quick Replies of `messages`, a reply.
fn offered(messages: &Value, label: &str) -> Value {
    fn find(value: &Value, label: &str) -> Option<Value> {
        match value {
            Value::Object(fields)
                if fields.contains_key("type")
                    && fields.get("label").and_then(Value::as_str) == Some(label) =>
            {
                Some(value.clone())
            }
            Value::Object(fields) => fields.values().find_map(|value| find(value, label)),
            Value::Array(values) => values.iter().find_map(|value| find(value, label)),
            _ => None,
        }
    }
    find(messages, label).unwrap_or_else(|| panic!("no \"{label}\" offered in {messages}"))
}

/// The event `id` of user `from` tapping `action`: its postback, or the text it sends as typed.
fn pressed(id: &str, from: &str, action: &Value) -> Value {
    match action["type"].as_str() {
        Some("postback") => tapped(id, from, action["data"].as_str().unwrap()),
        Some("message") => said(id, from, action["text"].as_str().unwrap()),
        _ => panic!("neither a postback nor text: {action}"),
    }
}

/// Stores `event` for `running` to handle, as the `n`th, and returns its reply, the `n`th.
async fn reply_to(s: &Setup, running: &Running, event: Value, n: usize) -> Value {
    store(&s.db, [event]).await;
    running.wakeup.notify();
    settled(&s.db, n).await;
    replied(s, n).await;
    s.replies()[n - 1].1.clone()
}

/// A seller who blocked the OA, so is pushed nothing (FR-NOT-02).
async fn blocked_seller(s: &Setup) -> i64 {
    let seller = user::ActiveModel {
        is_following: Set(false),
        ..testing::new_user("U-seller")
    };
    seller.insert(&s.db).await.unwrap().id
}

/// A returning buyer, สมชาย at ซอย 3 บ้านเลขที่ 45/2, and a product of `seller`'s shop, which
/// delivers and takes PromptPay: ไข่ไก่เบอร์ 2 at ฿110, 8 left.
async fn returning_buyer(s: &Setup, seller: i64) -> (i64, product::Model) {
    let alice = onboarded(&s.db, ALICE).await;
    let address = user::ActiveModel {
        id: Set(alice),
        display_name: Set(Some("สมชาย".to_owned())),
        zone: Set(Some("ซอย 3".to_owned())),
        house_number: Set(Some("45/2".to_owned())),
        ..Default::default()
    };
    address.update(&s.db).await.unwrap();
    let shop = shop::ActiveModel {
        promptpay_id: Set(Some("0812345678".to_owned())),
        ..testing::new_shop(seller)
    };
    let shop = shop.insert(&s.db).await.unwrap();
    let eggs = testing::new_category("ไข่/นม").insert(&s.db).await.unwrap();
    let product = testing::new_product(shop.id, eggs.id, "ไข่ไก่เบอร์ 2", Some(8));
    (alice, product.insert(&s.db).await.unwrap())
}

/// Appendix A.2's first 7 taps, from "ใส่ตะกร้า" on `product`'s card to checkout's summary,
/// each on a button or Quick Reply of the reply before: the summary.
async fn tapped_to_summary(s: &Setup, running: &Running, product: &product::Model) -> Value {
    let add = tapped("a1", ALICE, &format!("cart.add?product={}", product.id));
    let mut reply = reply_to(s, running, add, 1).await;
    let labels = ["2", "สั่งซื้อเลย", "ให้ไปส่ง +฿10", "ใช่", "เงินสดตอนรับของ", "ไม่มี"];
    for (n, label) in (2..).zip(labels) {
        let tap = pressed(&format!("a{n}"), ALICE, &offered(&reply, label));
        reply = reply_to(s, running, tap, n).await;
    }
    reply
}

/// Appendix A.2, NFR-USE-01, FR-CART-04 – FR-CART-08, AC-17, end to end: a returning buyer goes
/// from a product's card to a placed order in 8 taps, each on what the reply before offered,
/// without typing. Its seller blocked the OA, so no push is attempted.
#[tokio::test]
async fn a_returning_buyer_orders_in_eight_taps_end_to_end() {
    let s = setup().await;
    let seller = blocked_seller(&s).await;
    let (alice, product) = returning_buyer(&s, seller).await;
    let running = s.run();

    let summary = tapped_to_summary(&s, &running, &product).await;
    assert_eq!(summary[0]["altText"], "สรุปคำสั่งซื้อ รวม ฿230");
    let confirm = pressed("a8", ALICE, &offered(&summary, "ยืนยันสั่งซื้อ"));
    let placed = reply_to(&s, &running, confirm, 8).await;
    running.stop().await;

    let orders = crate::persistence::entity::order::Entity::find()
        .all(&s.db)
        .await
        .unwrap();
    assert_eq!(orders.len(), 1);
    let order = &orders[0];
    assert_eq!(
        (order.buyer_id, order.total),
        (alice, Money::from_baht(230))
    );
    assert!(
        order.order_number.ends_with("-0001"),
        "{}",
        order.order_number
    );
    assert_eq!(
        placed[0]["altText"],
        format!("สั่งซื้อเรียบร้อยค่ะ เลขที่ {} · รอร้านยืนยัน", order.order_number)
    );
    offered(&placed, "ยกเลิกออเดอร์");
    assert_eq!(s.replies().len(), 8);
    assert_eq!(pushes(&s), []);
}

/// AC-17, FR-ORD-10, end to end: a seller who blocked the OA is pushed nothing of a new order,
/// but, back in the chat, finds it in "ออเดอร์ร้าน", to accept from there, with every line in
/// "ดูรายละเอียด" and nothing yet of how to reach the buyer (FR-ORD-11).
#[tokio::test]
async fn a_blocked_seller_finds_the_order_in_shop_orders_end_to_end() {
    let s = setup().await;
    let bob = onboarded(&s.db, BOB).await;
    let following = |is_following| user::ActiveModel {
        id: Set(bob),
        is_following: Set(is_following),
        ..Default::default()
    };
    following(false).update(&s.db).await.unwrap();
    let (_, product) = returning_buyer(&s, bob).await;
    let running = s.run();

    let summary = tapped_to_summary(&s, &running, &product).await;
    let confirm = pressed("a8", ALICE, &offered(&summary, "ยืนยันสั่งซื้อ"));
    reply_to(&s, &running, confirm, 8).await;
    // Back, as their `follow` records
    following(true).update(&s.db).await.unwrap();
    let list = reply_to(&s, &running, tapped("b1", BOB, "shop_orders"), 9).await;
    let details = pressed("b2", BOB, &offered(&list, "ดูรายละเอียด"));
    let full = reply_to(&s, &running, details, 10).await;
    running.stop().await;

    assert_eq!(pushes(&s), []);
    let order = crate::persistence::entity::order::Entity::find()
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    let number = &order.order_number;
    assert_eq!(list[0]["text"], "ออเดอร์ \"ใหม่\" ของร้านค่ะ");
    assert_eq!(list[1]["altText"], "ออเดอร์ร้าน · ใหม่");
    assert_eq!(list[1]["contents"]["contents"].as_array().unwrap().len(), 1);
    offered(&list, "รับออเดอร์");
    offered(&list, "ปฏิเสธ");
    assert_eq!(full[0]["altText"], format!("ออเดอร์ {number}"));
    let full = full.to_string();
    for shown in ["ไข่ไก่เบอร์ 2", "2 แผง × ฿110", "฿230", "ส่งที่ ซอย 3", "สมชาย"]
    {
        assert!(full.contains(shown), "{shown} not shown: {full}");
    }
    assert!(!full.contains("45/2"), "the house number before accepting");
}

/// FR-LINE-07, FR-NOT-05, end to end: the order's card is essential, so when its reply token
/// fails it is pushed instead, and the push logged.
#[tokio::test]
async fn an_order_placed_is_pushed_when_its_reply_fails() {
    let s = setup().await;
    let seller = blocked_seller(&s).await;
    let (_, product) = returning_buyer(&s, seller).await;
    let running = s.run();

    let summary = tapped_to_summary(&s, &running, &product).await;
    s.line
        .script(REPLY, [Answer::error(400, "Invalid reply token")]);
    let confirm = pressed("a8", ALICE, &offered(&summary, "ยืนยันสั่งซื้อ"));
    store(&s.db, [confirm]).await;
    running.wakeup.notify();
    requested(&s, |path, _| path == PUSH).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let logs = loop {
        let logs = push_log::Entity::find().all(&s.db).await.unwrap();
        if !logs.is_empty() {
            break logs;
        }
        assert!(Instant::now() < deadline, "never logged");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    running.stop().await;

    let pushed = pushes(&s);
    assert_eq!(pushed.len(), 1);
    let (to, messages) = &pushed[0];
    assert_eq!(to, ALICE);
    let alt_text = messages[0]["altText"].as_str().unwrap();
    assert!(alt_text.starts_with("สั่งซื้อเรียบร้อยค่ะ เลขที่ IM-"), "{alt_text}");
    assert_eq!(logs.len(), 1);
    assert_eq!(
        (
            logs[0].push_type.as_str(),
            logs[0].status,
            logs[0].recipient.as_str()
        ),
        ("reply", push_log::Status::Sent, ALICE)
    );
}

/// The texts of the card `messages` holds, headline first.
fn card_lines(messages: &Value) -> Vec<&str> {
    let lines = messages[0]["contents"]["body"]["contents"].as_array();
    let lines = lines.expect("a card").iter();
    lines.map(|line| line["text"].as_str().unwrap()).collect()
}

/// Appendix A.4, FR-ORD-03, FR-ORD-08, FR-ORD-11, end to end: the seller is pushed the new
/// order with "รับออเดอร์" and "ปฏิเสธ", and accepts it with one tap: they are answered with where
/// to deliver and the buyer's phone, with "กำลังไปส่ง" and "ยกเลิกออเดอร์", and the buyer is then
/// pushed that the shop is preparing it, with "ขอยกเลิก".
#[tokio::test]
async fn a_seller_receives_and_accepts_an_order_end_to_end() {
    let s = setup().await;
    let bob = onboarded(&s.db, BOB).await;
    let (alice, product) = returning_buyer(&s, bob).await;
    let phone = user::ActiveModel {
        id: Set(alice),
        phone: Set(Some("0812345678".to_owned())),
        ..Default::default()
    };
    phone.update(&s.db).await.unwrap();
    let running = s.run();

    let summary = tapped_to_summary(&s, &running, &product).await;
    let confirm = pressed("a8", ALICE, &offered(&summary, "ยืนยันสั่งซื้อ"));
    reply_to(&s, &running, confirm, 8).await;
    requested(&s, |path, body| path == PUSH && body["to"] == BOB).await;
    let order = crate::persistence::entity::order::Entity::find()
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    let number = &order.order_number;
    let pushed = pushes(&s);
    assert_eq!(pushed.len(), 1);
    let (to, alert) = &pushed[0];
    assert_eq!(to, BOB);
    assert_eq!(alert[0]["altText"], format!("ออเดอร์ใหม่ค่ะ {number}"));
    assert_eq!(
        card_lines(alert),
        [
            format!("ออเดอร์ใหม่ค่ะ {number}").as_str(),
            "ไข่ไก่เบอร์ 2 × 2 แผง = ฿220",
            "ค่าส่ง ฿10 · รวม ฿230",
            "ส่งที่ ซอย 3 · เงินสดตอนรับของ",
            "ลูกค้า: สมชาย",
        ]
    );
    offered(alert, "ปฏิเสธ");

    let accept = pressed("b1", BOB, &offered(alert, "รับออเดอร์"));
    let accepted = reply_to(&s, &running, accept, 9).await;
    requested(&s, |path, body| path == PUSH && body["to"] == ALICE).await;
    running.stop().await;

    assert_eq!(
        card_lines(&accepted),
        [
            format!("รับออเดอร์ {number} แล้วค่ะ").as_str(),
            "ส่งที่ ซอย 3 บ้านเลขที่ 45/2",
            "ลูกค้า: สมชาย",
            "โทร 081-234-5678",
        ]
    );
    offered(&accepted, "กำลังไปส่ง");
    offered(&accepted, "ยกเลิกออเดอร์");
    let told = pushes(&s);
    assert_eq!(told.len(), 2);
    let (to, told) = &told[1];
    assert_eq!(to, ALICE);
    assert_eq!(
        told[0]["altText"],
        format!("ร้านป้าแดงรับออเดอร์ {number} แล้วค่ะ กำลังเตรียมของ")
    );
    offered(told, "ขอยกเลิก");
    let order = crate::persistence::entity::order::Entity::find_by_id(order.id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        order.status,
        crate::persistence::entity::order::Status::Accepted
    );
    // The seller is answered, then the buyer told
    let paths: Vec<_> = s
        .line
        .requests()
        .into_iter()
        .map(|request| {
            let body = body(&request);
            let to = body["to"].as_str().or(body["replyToken"].as_str());
            (request.path, to.map(str::to_owned))
        })
        .collect();
    let at = |path: &str, to: &str| {
        paths
            .iter()
            .position(|wanted| wanted.0 == path && wanted.1.as_deref() == Some(to))
            .unwrap()
    };
    assert!(at(REPLY, &token("b1")) < at(PUSH, ALICE), "{paths:?}");
}

/// The acceptance criterion of #35, FR-PAY-07, FR-PAY-08, end to end: "ได้รับเงินแล้ว" on an
/// order handed over unpaid is refused to its buyer, and records it paid for its seller, the
/// buyer then being told.
#[tokio::test]
async fn only_the_seller_records_an_order_paid_end_to_end() {
    use crate::persistence::entity::order;
    let s = setup().await;
    let bob = onboarded(&s.db, BOB).await;
    let (alice, product) = returning_buyer(&s, bob).await;
    let handed_over = order::ActiveModel {
        status: Set(order::Status::Completed),
        ..testing::new_order(
            alice,
            product.shop_id,
            "IM-20261025-0001",
            crate::persistence::random_id(),
        )
    };
    let placed = handed_over.insert(&s.db).await.unwrap();
    let running = s.run();

    let paid = format!("order.paid?order={}", placed.id);
    let refused = reply_to(&s, &running, tapped("a1", ALICE, &paid), 1).await;
    let recorded = reply_to(&s, &running, tapped("b1", BOB, &paid), 2).await;
    requested(&s, |path, body| path == PUSH && body["to"] == ALICE).await;
    running.stop().await;

    let denied = s.templates.refused(Denied::NotAllowed, None);
    assert_eq!(refused, json!([denied]));
    assert_eq!(
        card_lines(&recorded),
        ["บันทึกว่าได้รับเงินออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
    let order = order::Entity::find_by_id(placed.id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(order.payment_status, order::PaymentStatus::Paid);
    let told = pushes(&s);
    assert_eq!(told.len(), 1);
    assert_eq!(
        card_lines(&told[0].1),
        ["ร้านป้าแดงยืนยันว่าได้รับเงินค่าออเดอร์ IM-20261025-0001 แล้วค่ะ"]
    );
}

/// FR-LINE-07: a reply that isn't essential is dropped when its reply token fails, as a push
/// counts against the monthly quota; an essential one is pushed, and the push logged.
#[tokio::test]
async fn only_an_essential_reply_is_pushed_when_its_token_fails() {
    let s = setup().await;
    let worker = Worker::new(
        &TaskTracker::new(),
        CancellationToken::new(),
        s.context(&s.pool, 20),
        Wakeup::default(),
        1,
        FAST,
    );
    let answer = |token: &str, essential: bool| super::Answer {
        handled: Handled {
            reply: Some(Reply {
                token: Some(token.to_owned()),
                to: ALICE.to_owned(),
                messages: vec![Message::text("สั่งซื้อเรียบร้อยค่ะ")],
                essential,
            }),
            ..Handled::default()
        },
        received_at: Utc::now(),
        span: Span::none(),
    };
    let expired = || Answer::error(400, "Invalid reply token");
    s.line.script(REPLY, [expired(), expired()]);

    worker.send(answer("optional", false)).await;
    assert_eq!(pushes(&s), []);
    worker.send(answer("essential", true)).await;
    assert_eq!(
        pushes(&s),
        [(
            ALICE.to_owned(),
            json!([{ "type": "text", "text": "สั่งซื้อเรียบร้อยค่ะ" }])
        )]
    );
    let logs = push_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(
        (logs[0].push_type.as_str(), logs[0].status),
        ("reply", push_log::Status::Sent)
    );
}

/// FR-PROD-09, end to end: a product listed with a prohibited keyword is held for review. The
/// seller is answered that it is under review, and the admins are pushed the alert after.
#[tokio::test]
async fn a_suspicious_product_is_held_end_to_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = onboarded(&s.db, ALICE).await;
    let mut seller = user::Entity::find_by_id(alice)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    seller.menu_roles = Set(Some(MenuRoles::Seller));
    seller.update(&s.db).await.unwrap();
    let bob = onboarded(&s.db, BOB).await;
    let mut admin = user::Entity::find_by_id(bob)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    admin.is_admin = Set(true);
    admin.menu_roles = Set(Some(MenuRoles::Admin));
    admin.update(&s.db).await.unwrap();
    testing::new_shop(alice).insert(&s.db).await.unwrap();
    testing::new_category("ขนม/ของหวาน")
        .insert(&s.db)
        .await
        .unwrap();

    store(
        &s.db,
        [
            tapped("a1", ALICE, "add_product"),
            said("a2", ALICE, "ข้าม"),
            said("a3", ALICE, "คุกกี้เนยสด"),
            said("a4", ALICE, "50"),
            said("a5", ALICE, "ถุง"),
            said("a6", ALICE, "ขนม/ของหวาน"),
            said("a7", ALICE, "10"),
            said("a8", ALICE, "ผสมกัญชา หอมมาก"),
            tapped("a9", ALICE, "add_product.submit?r=7"),
        ],
    )
    .await;
    let mut context = s.context(&s.pool, 20);
    let config = Arc::get_mut(&mut context.config).expect("not shared yet");
    config.catalog.prohibited_keywords = vec!["กัญชา".to_owned()];
    let running = s.run_with(context, 2);
    settled(&s.db, 9).await;
    replied(&s, 9).await;
    running.stop().await;

    let replies: HashMap<_, _> = s.replies().into_iter().collect();
    let next = vec![
        Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone()),
        Item::AddProduct
            .postback()
            .button(labels::ADD_ANOTHER.clone()),
    ];
    assert_eq!(
        replies[&token("a9")],
        json!([t.product_held("คุกกี้เนยสด", next)])
    );
    let product = product::Entity::find().one(&s.db).await.unwrap().unwrap();
    assert_eq!(product.status, product::Status::Held);
    let reports = report::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(
        reports
            .iter()
            .map(|report| (report.reporter_id, report.target_id))
            .collect::<Vec<_>>(),
        [(None, product.id)]
    );
    let logs = push_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(
        logs.iter()
            .map(|log| (log.push_type.as_str(), log.recipient.as_str(), log.status))
            .collect::<Vec<_>>(),
        [("admin.held_for_review", BOB, push_log::Status::Sent)]
    );
}
