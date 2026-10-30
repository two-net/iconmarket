//! A fake LINE for tests: two local HTTP servers, for the API and content hosts, that record
//! every request and answer from a script. Pushes follow LINE's retry-key rule, so tests can
//! count deliveries.

use std::collections::{HashMap, VecDeque};
use std::future::IntoFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use url::Url;

use super::Client;
use crate::config::Secret;

pub const TOKEN: &str = "channel-access-token-value";
pub const USER: &str = "U0123456789abcdef0123456789abcdef";
pub const GROUP: &str = "C0123456789abcdef0123456789abcdef";
pub const PUSH: &str = "/v2/bot/message/push";
pub const REPLY: &str = "/v2/bot/message/reply";
pub const BOT_INFO: &str = "/v2/bot/info";
pub const QUOTA: &str = "/v2/bot/message/quota";
pub const QUOTA_CONSUMPTION: &str = "/v2/bot/message/quota/consumption";

/// What the fake answers to one request.
#[derive(Clone)]
pub struct Answer {
    status: StatusCode,
    content_type: &'static str,
    body: Vec<u8>,
    /// Waited before answering, after the request is recorded and any push delivered
    delay: Duration,
    /// Whether a push answered this way was delivered
    delivers: bool,
}

impl Answer {
    pub fn json(status: u16, body: Value) -> Self {
        let status = StatusCode::from_u16(status).unwrap();
        Self {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
            delay: Duration::ZERO,
            delivers: status.is_success(),
        }
    }

    pub fn ok() -> Self {
        Self::json(200, json!({}))
    }

    /// LINE's error body
    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, json!({ "message": message }))
    }

    pub fn bytes(content_type: &'static str, body: &[u8]) -> Self {
        Self {
            content_type,
            body: body.to_vec(),
            ..Self::ok()
        }
    }

    /// The push was delivered, but this answer says otherwise, as when an answer is lost
    pub fn after_delivering(self) -> Self {
        Self {
            delivers: true,
            ..self
        }
    }

    pub fn slow(self) -> Self {
        // Longer than the test client waits
        Self {
            delay: Duration::from_secs(3),
            ..self
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    /// `api` or `data`
    pub host: &'static str,
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Request {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

#[derive(Default)]
struct Line {
    requests: Vec<Request>,
    /// Answers still to give, by path; then 200 `{}`
    script: HashMap<String, VecDeque<Answer>>,
    /// Retry keys of delivered pushes, with the request that delivered each
    accepted: HashMap<String, String>,
    deliveries: usize,
}

#[derive(Clone)]
pub struct Fake {
    line: Arc<Mutex<Line>>,
    api: Url,
    data: Url,
}

impl Fake {
    pub async fn start() -> Self {
        let line = Arc::new(Mutex::new(Line::default()));
        let api = serve(line.clone(), "api").await;
        let data = serve(line.clone(), "data").await;
        Self { line, api, data }
    }

    pub fn client(&self) -> Client {
        Client::for_test(&Secret::new(TOKEN), self.api.clone(), self.data.clone())
    }

    pub fn script(&self, path: &str, answers: impl IntoIterator<Item = Answer>) {
        let mut line = self.line.lock().unwrap();
        line.script
            .entry(path.to_owned())
            .or_default()
            .extend(answers);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.line.lock().unwrap().requests.clone()
    }

    pub fn deliveries(&self) -> usize {
        self.line.lock().unwrap().deliveries
    }
}

async fn serve(line: Arc<Mutex<Line>>, host: &'static str) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().fallback(answer).with_state((line, host));
    tokio::spawn(axum::serve(listener, app).into_future());
    url
}

async fn answer(
    State((line, host)): State<(Arc<Mutex<Line>>, &'static str)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (answer, request_id, accepted) = {
        let mut line = line.lock().unwrap();
        let request_id = format!("req-{}", line.requests.len() + 1);
        let path = uri.path().to_owned();
        let retry_key = headers
            .get("x-line-retry-key")
            .map(|key| key.to_str().unwrap().to_owned());
        line.requests.push(Request {
            host,
            method,
            path: path.clone(),
            headers,
            body,
        });

        let accepted = retry_key
            .as_ref()
            .and_then(|key| line.accepted.get(key).cloned());
        let answer = match &accepted {
            Some(_) => Answer::error(409, "The retry key is already accepted"),
            None => line
                .script
                .get_mut(&path)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(Answer::ok),
        };
        if path == PUSH && accepted.is_none() && answer.delivers {
            line.deliveries += 1;
            if let Some(key) = retry_key {
                line.accepted.insert(key, request_id.clone());
            }
        }
        (answer, request_id, accepted)
    };

    tokio::time::sleep(answer.delay).await;
    let mut response = Response::builder()
        .status(answer.status)
        .header("content-type", answer.content_type)
        .header("x-line-request-id", &request_id);
    if let Some(accepted) = accepted {
        response = response.header("x-line-accepted-request-id", accepted);
    }
    response.body(answer.body.into()).unwrap()
}
