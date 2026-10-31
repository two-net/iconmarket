//! A fake LINE, on the two hosts the app calls. Unlike the tests' fake it keeps nothing but what
//! is waiting, and takes as long as the real one, so that a run isn't flattered by an instant
//! LINE. A reply counts as delivered once that wait is over.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::{Random, journeys};

const REPLY: &str = "/v2/bot/message/reply";
const PUSH: &str = "/v2/bot/message/push";

/// A reply LINE delivered.
pub struct Delivered {
    pub at: Instant,
    pub messages: Value,
}

/// What can go wrong on LINE's side of a run.
#[derive(Default)]
pub struct Oddities {
    pub replies_without_a_waiter: AtomicU64,
    pub pushes: AtomicU64,
}

pub struct Line {
    /// Who waits for the reply with this token
    replies: Mutex<HashMap<String, oneshot::Sender<Delivered>>>,
    /// Buyers waiting for the push that lets them send their slip
    slips: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Pushes for sellers to act on, with their recipient
    sellers: mpsc::UnboundedSender<(String, Value)>,
    /// What a user's photo downloads as
    content: Bytes,
    /// How long a call takes, in ms
    latency: (u64, u64),
    random: Mutex<Random>,
    pub oddities: Oddities,
}

impl Line {
    pub fn new(
        content: Vec<u8>,
        latency: (u64, u64),
        sellers: mpsc::UnboundedSender<(String, Value)>,
    ) -> Self {
        Self {
            replies: Mutex::new(HashMap::new()),
            slips: Mutex::new(HashMap::new()),
            sellers,
            content: content.into(),
            latency,
            random: Mutex::new(Random::new(7)),
            oddities: Oddities::default(),
        }
    }

    pub fn expect_reply(&self, token: &str, waiter: oneshot::Sender<Delivered>) {
        self.replies
            .lock()
            .unwrap()
            .insert(token.to_owned(), waiter);
    }

    pub fn expect_slip_push(&self, to: &str, waiter: oneshot::Sender<Value>) {
        self.slips.lock().unwrap().insert(to.to_owned(), waiter);
    }

    /// One in a hundred calls is much slower, as on any network.
    async fn wait(&self) {
        let ms = {
            let mut random = self.random.lock().unwrap();
            if random.between(0, 100) == 0 {
                1_000
            } else {
                random.between(self.latency.0, self.latency.1 + 1)
            }
        };
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    fn replied(&self, body: &Value) -> StatusCode {
        let token = body["replyToken"].as_str().unwrap_or_default();
        let waiter = self.replies.lock().unwrap().remove(token);
        if let Some(waiter) = waiter {
            let _ = waiter.send(Delivered {
                at: Instant::now(),
                messages: body["messages"].clone(),
            });
            return StatusCode::OK;
        }
        self.oddities
            .replies_without_a_waiter
            .fetch_add(1, Ordering::Relaxed);
        StatusCode::BAD_REQUEST
    }

    fn pushed(&self, body: &Value) {
        self.oddities.pushes.fetch_add(1, Ordering::Relaxed);
        let to = body["to"].as_str().unwrap_or_default().to_owned();
        let messages = &body["messages"];
        if !journeys::offered_any(messages, "ส่งสลิป").is_empty() {
            let waiter = self.slips.lock().unwrap().remove(&to);
            if let Some(waiter) = waiter {
                let _ = waiter.send(messages.clone());
            }
        }
        let to_act_on = ["รับออเดอร์", "ได้รับเงินแล้ว"]
            .iter()
            .any(|label| !journeys::offered_any(messages, label).is_empty());
        if to_act_on {
            let _ = self.sellers.send((to, messages.clone()));
        }
    }
}

/// The JSON LINE answers `path` with; replies and pushes are handled before this.
fn answer(method: &Method, path: &str) -> Value {
    let posted = method == Method::POST;
    if path == "/v2/bot/richmenu" && posted {
        return json!({ "richMenuId": "richmenu-loadtest" });
    }
    match path {
        "/v2/bot/richmenu/list" => json!({ "richmenus": [] }),
        "/v2/bot/richmenu/alias/list" => json!({ "aliases": [] }),
        "/v2/bot/info" => json!({ "basicId": "@loadtest" }),
        "/v2/bot/message/quota" => json!({ "type": "none" }),
        "/v2/bot/message/quota/consumption" => json!({ "totalUsage": 0 }),
        _ if path.starts_with("/v2/bot/profile/") => json!({ "displayName": "ผู้ใช้ทดสอบ" }),
        _ => json!({}),
    }
}

async fn api(State(line): State<Arc<Line>>, method: Method, uri: Uri, body: Bytes) -> Response {
    line.wait().await;
    let path = uri.path();
    let json = |value: Value| {
        (
            [
                ("content-type", "application/json"),
                ("x-line-request-id", "loadtest"),
            ],
            value.to_string(),
        )
    };
    if path == REPLY {
        let body: Value = serde_json::from_slice(&body).unwrap_or_default();
        return (line.replied(&body), json(json!({}))).into_response();
    }
    if path == PUSH {
        let body: Value = serde_json::from_slice(&body).unwrap_or_default();
        line.pushed(&body);
        return json(json!({ "sentMessages": [] })).into_response();
    }
    json(answer(&method, path)).into_response()
}

/// Content for any message ID, and nothing to do for anything else the data host takes.
async fn data(State(line): State<Arc<Line>>, uri: Uri) -> Response {
    line.wait().await;
    if uri.path().ends_with("/content") {
        return (
            [("content-type", "image/jpeg")],
            Bytes::clone(&line.content),
        )
            .into_response();
    }
    ([("content-type", "application/json")], "{}").into_response()
}

/// Serves `line` at the two addresses until the process ends.
pub async fn serve(line: Arc<Line>, api_at: &str, data_at: &str) -> anyhow::Result<()> {
    let hosts = [
        (api_at, Router::new().fallback(self::api)),
        (data_at, Router::new().fallback(self::data)),
    ];
    for (at, router) in hosts {
        let listener = TcpListener::bind(at).await?;
        let router = router.with_state(line.clone());
        tokio::spawn(axum::serve(listener, router).into_future());
    }
    Ok(())
}
