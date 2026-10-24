//! A fake provider for tests: a local HTTP server that records every exchange. It answers
//! from a script, or passes each request on to the real API and keeps what came back, which is
//! how the contract recordings are made.

use std::collections::VecDeque;
use std::future::IntoFuture;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tokio::net::TcpListener;
use url::Url;

use crate::tls;

/// One request and the answer it got.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub path: String,
    pub headers: HeaderMap,
    pub request: Value,
    pub status: u16,
    pub response: Value,
}

#[derive(Default)]
struct Provider {
    /// Answers still to give, in order
    script: VecDeque<(u16, Value)>,
    /// The real API to pass requests on to, instead of the script
    upstream: Option<(reqwest::Client, Url)>,
    exchanges: Vec<Exchange>,
}

#[derive(Clone)]
pub struct Fake {
    provider: Arc<Mutex<Provider>>,
    /// Give this to the adapter as `ai.baseUrl`
    pub url: Url,
}

impl Fake {
    pub async fn start() -> Self {
        Self::serve(Provider::default()).await
    }

    /// A fake that passes every request on to `upstream`, the provider's own API, with its
    /// headers, and records the answer.
    pub async fn forwarding(upstream: Url) -> Self {
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls::client_config().unwrap())
            .no_proxy()
            .build()
            .unwrap();
        Self::serve(Provider {
            upstream: Some((client, upstream)),
            ..Provider::default()
        })
        .await
    }

    async fn serve(provider: Provider) -> Self {
        let provider = Arc::new(Mutex::new(provider));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().fallback(answer).with_state(provider.clone());
        tokio::spawn(axum::serve(listener, app).into_future());
        Self { provider, url }
    }

    /// Answers the next request with `status` and `body`.
    pub fn answer(&self, status: u16, body: Value) {
        self.provider
            .lock()
            .unwrap()
            .script
            .push_back((status, body));
    }

    pub fn exchanges(&self) -> Vec<Exchange> {
        self.provider.lock().unwrap().exchanges.clone()
    }
}

async fn answer(
    State(provider): State<Arc<Mutex<Provider>>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let upstream = provider.lock().unwrap().upstream.clone();
    let (status, response) = match upstream {
        Some((client, base)) => forward(&client, &base, &uri, &headers, body).await,
        None => provider
            .lock()
            .unwrap()
            .script
            .pop_front()
            .expect("the fake provider's script has no answer left"),
    };
    provider.lock().unwrap().exchanges.push(Exchange {
        path,
        headers,
        request,
        status,
        response: response.clone(),
    });
    let status = StatusCode::from_u16(status).unwrap();
    (status, axum::Json(response)).into_response()
}

async fn forward(
    client: &reqwest::Client,
    base: &Url,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> (u16, Value) {
    let path = uri
        .path_and_query()
        .map_or(uri.path(), |path| path.as_str());
    let url = base.join(path.trim_start_matches('/')).unwrap();
    let mut forwarded = headers.clone();
    forwarded.remove("host");
    forwarded.remove("content-length");
    let response = client
        .post(url)
        .headers(forwarded)
        .body(body)
        .send()
        .await
        .expect("the provider answers");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}
