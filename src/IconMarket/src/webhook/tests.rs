//! The webhook endpoint end to end: signature verification (AC-01) and idempotent storing
//! (AC-02, FR-LINE-04), then the worker's one reply to an event LINE redelivers. The tests on a
//! migrated database each start a throwaway PostgreSQL, so they need Docker.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{TimeZone, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, PaginatorTrait, QueryOrder};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

use super::*;
use crate::config::{self, Secret};
use crate::http::{self, AppState};
use crate::line::testing::{Answer, Fake, REPLY, USER};
use crate::notifications::templates::Templates;
use crate::persistence::entity::user;
use crate::persistence::entity::webhook_event::{self, Status};
use crate::persistence::{orm, testing};
use crate::{ai, dispatcher};

const SECRET: &str = "channel-secret";

fn app(db: PgPool) -> Router {
    app_waking(db, dispatcher::Wakeup::default())
}

fn app_waking(db: PgPool, wakeup: dispatcher::Wakeup) -> Router {
    http::router(AppState {
        db,
        webhook: Verifier::new(&Secret::new(SECRET)),
        wakeup,
    })
}

/// The signature LINE sends with `body`.
fn sign(body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

async fn post(db: &PgPool, body: &str, signature: Option<&str>) -> StatusCode {
    post_to(app(db.clone()), body, signature).await
}

async fn post_to(app: Router, body: &str, signature: Option<&str>) -> StatusCode {
    let mut request = Request::builder().method(Method::POST).uri("/webhook/line");
    if let Some(signature) = signature {
        request = request.header(SIGNATURE, signature);
    }
    let request = request.body(Body::from(body.to_owned())).unwrap();
    app.oneshot(request).await.unwrap().status()
}

/// Posts `body` signed, as LINE delivers it.
async fn deliver(db: &PgPool, body: &str) -> StatusCode {
    post(db, body, Some(&sign(body))).await
}

fn body(events: &[&Value]) -> String {
    json!({ "destination": "U0123456789abcdef0123456789abcdef", "events": events }).to_string()
}

fn message(id: &str, user: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "mode": "active",
        "timestamp": 1_761_300_000_000_u64,
        "source": { "type": "user", "userId": user },
        "webhookEventId": id,
        "deliveryContext": { "isRedelivery": false },
        "replyToken": "38ef843bde154d9b91c21320ffd17a0f",
        "message": { "id": "468789577898262530", "type": "text", "quoteToken": "q", "text": text },
    })
}

fn join(id: &str, group: &str) -> Value {
    json!({
        "type": "join",
        "mode": "active",
        "timestamp": 1_761_300_000_000_u64,
        "source": { "type": "group", "groupId": group },
        "webhookEventId": id,
        "deliveryContext": { "isRedelivery": false },
        "replyToken": "0f3779fba3b349968c5d07db31eab56f",
    })
}

/// `event` as LINE sends it again.
fn redelivered(mut event: Value) -> Value {
    event["deliveryContext"]["isRedelivery"] = json!(true);
    event
}

async fn inbox(db: &PgPool) -> Vec<webhook_event::Model> {
    webhook_event::Entity::find()
        .order_by_asc(webhook_event::Column::Id)
        .all(&orm(db.clone()))
        .await
        .unwrap()
}

/// Getting 400 rather than 503 shows the database, which is down, was never reached.
#[tokio::test]
async fn requests_without_a_valid_signature_are_refused_before_the_database() {
    let db = testing::unreachable();
    let body = body(&[&message("01J1", "U1", "hi")]);
    let other_body = sign(&body.replace("hi", "ho"));
    let empty_body = sign("");
    let lowercase = sign(&body).to_lowercase();
    for signature in [
        None,
        Some(""),
        Some("not base64!"),
        Some(other_body.as_str()),
        Some(empty_body.as_str()),
        Some(lowercase.as_str()),
    ] {
        let status = post(&db, &body, signature).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{signature:?}");
    }
}

#[tokio::test]
async fn a_request_without_events_is_acknowledged_without_the_database() {
    let status = deliver(&testing::unreachable(), &body(&[])).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_signed_body_that_is_not_from_line_is_refused() {
    let db = testing::unreachable();
    for body in ["", "not json", r#"{"events":[{"type":"follow"}]}"#] {
        assert_eq!(deliver(&db, body).await, StatusCode::BAD_REQUEST, "{body}");
    }
}

/// NFR-REL-01
#[tokio::test]
async fn events_are_not_acknowledged_while_the_database_is_down() {
    let status = deliver(
        &testing::unreachable(),
        &body(&[&message("01J1", "U1", "hi")]),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// AC-01
#[tokio::test]
async fn a_request_with_an_invalid_signature_stores_nothing() {
    let (_container, db) = testing::migrated().await;
    let body = body(&[&message("01J1", "U1", "hi")]);
    let forged = sign(&body.replace("hi", "ho"));

    assert_eq!(
        post(&db, &body, Some(&forged)).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(post(&db, &body, None).await, StatusCode::BAD_REQUEST);
    assert!(inbox(&db).await.is_empty());
}

#[tokio::test]
async fn events_are_stored_pending_before_they_are_acknowledged() {
    let (_container, db) = testing::migrated().await;
    let events = [message("01J1", "U1", "เอาไข่ไก่ 2 แผง"), join("01J2", "C1")];
    assert_eq!(
        deliver(&db, &body(&[&events[0], &events[1]])).await,
        StatusCode::OK
    );

    let inbox = inbox(&db).await;
    let stored: Vec<_> = inbox
        .iter()
        .map(|row| {
            let key = (row.event_id.as_str(), row.user_key.as_str());
            let state = (
                row.status,
                row.attempts,
                row.processed_at,
                row.error.as_deref(),
            );
            (key, row.event_type.as_str(), &row.payload, state)
        })
        .collect();
    let pending = (Status::Pending, 0, None, None);
    assert_eq!(
        stored,
        [
            (("01J1", "U1"), "message", &events[0], pending),
            (("01J2", "C1"), "join", &events[1], pending),
        ]
    );
}

/// AC-02, FR-LINE-04
#[tokio::test]
async fn a_redelivered_event_that_was_processed_changes_nothing() {
    let (_container, db) = testing::migrated().await;
    let event = message("01J1", "U1", "hi");
    assert_eq!(deliver(&db, &body(&[&event])).await, StatusCode::OK);

    // The dispatcher processes it
    let [stored] = &inbox(&db).await[..] else {
        panic!("one event is stored");
    };
    let mut processed = stored.clone().into_active_model();
    processed.status = Set(Status::Done);
    processed.attempts = Set(1);
    processed.processed_at = Set(Some(Utc.with_ymd_and_hms(2026, 10, 24, 9, 0, 0).unwrap()));
    let processed = processed.update(&orm(db.clone())).await.unwrap();

    // LINE redelivers it alone, then with a new event
    let again = redelivered(event);
    let next = message("01J2", "U1", "hello");
    assert_eq!(deliver(&db, &body(&[&again])).await, StatusCode::OK);
    assert_eq!(deliver(&db, &body(&[&again, &next])).await, StatusCode::OK);

    let inbox = inbox(&db).await;
    assert_eq!(inbox.len(), 2, "{inbox:#?}");
    assert_eq!(inbox[0], processed);
    assert_eq!(
        (inbox[1].event_id.as_str(), inbox[1].status),
        ("01J2", Status::Pending)
    );
}

/// The tokens of the replies LINE got, in order.
fn reply_tokens(line: &Fake) -> Vec<String> {
    line.requests()
        .iter()
        .filter(|request| request.path == REPLY)
        .map(|request| request.json()["replyToken"].as_str().unwrap().to_owned())
        .collect()
}

/// `id`'s text message, with a reply token of its own.
fn said(id: &str, text: &str) -> Value {
    let mut event = message(id, USER, text);
    event["replyToken"] = json!(format!("reply-{id}"));
    event
}

/// Waits for `n` replies, for less than the worker waits between looks at the inbox, so that
/// only the webhook waking it gets them there in time.
async fn replied(line: &Fake, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while reply_tokens(line).len() < n {
        assert!(Instant::now() < deadline, "{:?}", reply_tokens(line));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// AC-02 as one path, from a signed request over HTTP to LINE's reply (NFR-REL-02): the webhook
/// wakes the worker, which answers once, and LINE redelivering the event changes nothing. A
/// new event then shows the redelivered one wasn't left waiting, as a user's events go in order.
#[tokio::test]
async fn an_event_is_answered_once_however_often_line_redelivers_it() {
    let (_container, db) = testing::migrated().await;
    let line = Fake::start().await;
    line.script(
        &format!("/v2/bot/profile/{USER}"),
        [Answer::json(200, json!({ "displayName": "บ๊อบ" }))],
    );
    let mut config = config::sample();
    config.ai.primary = None;
    let wakeup = dispatcher::Wakeup::default();
    let (tracker, stop) = (TaskTracker::new(), CancellationToken::new());
    let context = dispatcher::Context {
        db: orm(db.clone()),
        line: line.client(),
        ai: ai::Gateway::new(&config.ai, chrono_tz::Asia::Bangkok, db.clone()),
        templates: Templates::new(&config.app),
        config: Arc::new(config),
        rich_menus: None,
    };
    dispatcher::start(&tracker, stop.clone(), context, wakeup.clone());
    let app = app_waking(db.clone(), wakeup);
    let deliver = |events: &[&Value]| {
        let body = body(events);
        let app = app.clone();
        async move { post_to(app, &body, Some(&sign(&body))).await }
    };

    let first = said("01J1", "สวัสดี");
    assert_eq!(deliver(&[&first]).await, StatusCode::OK);
    replied(&line, 1).await;
    // An event is answered once it has committed
    let handled = inbox(&db).await;
    assert_eq!((handled[0].status, handled[0].attempts), (Status::Done, 1));

    let again = redelivered(first);
    assert_eq!(deliver(&[&again]).await, StatusCode::OK);
    let next = said("01J2", "ขอไข่ไก่");
    assert_eq!(deliver(&[&again, &next]).await, StatusCode::OK);
    replied(&line, 2).await;
    stop.cancel();
    tracker.close();
    tracker.wait().await;

    assert_eq!(reply_tokens(&line), ["reply-01J1", "reply-01J2"]);
    let inbox = inbox(&db).await;
    assert_eq!(inbox.len(), 2, "{inbox:#?}");
    assert_eq!(inbox[0], handled[0]);
    let users = user::Entity::find().count(&orm(db)).await.unwrap();
    assert_eq!(users, 1);
}

#[tokio::test]
async fn an_event_delivered_twice_at_once_is_stored_once() {
    let (_container, db) = testing::migrated().await;
    let event = message("01J1", "U1", "hi");
    let first = body(&[&event]);
    let second = body(&[&redelivered(event.clone())]);
    let repeated = body(&[&event, &event]);

    let statuses = tokio::join!(
        deliver(&db, &first),
        deliver(&db, &second),
        deliver(&db, &repeated),
    );
    assert_eq!(statuses, (StatusCode::OK, StatusCode::OK, StatusCode::OK));
    let ids: Vec<_> = inbox(&db)
        .await
        .into_iter()
        .map(|row| row.event_id)
        .collect();
    assert_eq!(ids, ["01J1"]);
}
