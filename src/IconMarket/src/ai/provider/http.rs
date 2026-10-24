//! The connection to a provider, shared by the adapters: TLS, the key, timeouts and how an
//! unsuccessful answer becomes an [`Error`].

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use url::Url;

use super::Error;
use crate::config::Secret;
use crate::tls;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Shown instead of the provider's text for 401, 403 and any refusal about the key, which can
/// quote part of it (CFG-06): xAI answers a wrong key with 400
const KEY_REJECTED: &str = "the API key was rejected";
/// The provider's message is cut to this many characters
const MESSAGE_CHARS: usize = 200;
/// Codes and types of a 429 that means billing, not the rate limit: trying again can't help
const BILLING: [&str; 4] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "billing_hard_limit_reached",
    "billing_not_active",
];

/// A provider's API. Cheap to clone: clones share one connection pool.
#[derive(Clone, Debug)]
pub struct Http {
    client: reqwest::Client,
    base: Url,
}

/// How the key is sent.
pub enum Auth {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// The key as the value of this header
    Header(&'static str),
}

impl Http {
    /// A client for the API under `base`, sending the key as `auth` says and `headers` on
    /// every request.
    pub fn new(
        base: Url,
        key: &Secret,
        auth: Auth,
        headers: &[(&'static str, &'static str)],
    ) -> Result<Self, String> {
        let (name, value) = match auth {
            Auth::Bearer => (
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", key.expose()),
            ),
            Auth::Header(name) => (HeaderName::from_static(name), key.expose().to_owned()),
        };
        let mut value = HeaderValue::try_from(value)
            .map_err(|_| "ai.apiKey can't be sent in an HTTP header".to_owned())?;
        // Masked in `Debug` (CFG-06)
        value.set_sensitive(true);
        let mut defaults = HeaderMap::from_iter([(name, value)]);
        for (name, value) in headers {
            defaults.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let tls = tls::client_config().map_err(|e| format!("{e:#}"))?;
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            // Otherwise reqwest reads HTTP_PROXY and friends (CFG-02)
            .no_proxy()
            .default_headers(defaults)
            .user_agent(concat!("icon-market/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot set up the AI client: {e}"))?;
        Ok(Self { client, base })
    }

    /// `segments` under the base URL, each percent-encoded.
    pub fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http(s) base URL")
            .pop_if_empty()
            .extend(segments);
        url
    }

    /// Posts `body` and returns the provider's JSON answer. Gives up after `timeout`, which
    /// the gateway enforces as well.
    pub async fn post(&self, url: Url, body: &Value, timeout: Duration) -> Result<Value, Error> {
        let response = self
            .client
            .post(url)
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(network)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(network)?;
        let answer: Option<Value> = serde_json::from_slice(&bytes).ok();
        if status.is_success() {
            return answer.ok_or_else(|| Error::Malformed("the answer is not JSON".to_owned()));
        }
        Err(refusal(status, answer.as_ref()))
    }
}

/// The error for an unsuccessful `status` with the provider's error `body`.
fn refusal(status: StatusCode, body: Option<&Value>) -> Error {
    let error = body.and_then(|body| body.get("error"));
    let says = |name: &str| {
        error
            .and_then(|error| error.get(name))
            .and_then(Value::as_str)
            .is_some_and(|value| BILLING.contains(&value))
    };
    let message = body.and_then(message).unwrap_or_default();
    match status.as_u16() {
        429 if !says("code") && !says("type") => Error::RateLimited,
        status @ 500..=599 => Error::Server { status },
        status if matches!(status, 401 | 403) || message.to_lowercase().contains("api key") => {
            Error::Refused {
                status,
                message: KEY_REJECTED.to_owned(),
            }
        }
        status => Error::Refused { status, message },
    }
}

/// The provider's description of the error, shortened: `error.message` for Claude, Gemini
/// and OpenAI; xAI may put a string in `error`.
fn message(body: &Value) -> Option<String> {
    let text = body
        .pointer("/error/message")
        .or_else(|| body.get("error"))
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)?;
    let mut message: String = text.chars().take(MESSAGE_CHARS).collect();
    if message.len() < text.len() {
        message.push('…');
    }
    Some(message)
}

/// The URL is dropped: it names the model, which the call log has anyway, and nothing else
/// useful.
fn network(error: reqwest::Error) -> Error {
    let error = error.without_url();
    // reqwest says only "error sending request"; the reason is in its sources
    let mut text = error.to_string();
    let mut cause = std::error::Error::source(&error);
    while let Some(e) = cause {
        text.push_str(&format!(": {e}"));
        cause = e.source();
    }
    Error::Network(text)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn refused(status: u16, body: Value) -> Error {
        refusal(StatusCode::from_u16(status).unwrap(), Some(&body))
    }

    #[test]
    fn rate_limits_and_server_errors_can_be_retried() {
        let body = json!({ "error": { "message": "slow down", "type": "rate_limit_error" } });
        assert!(matches!(refused(429, body), Error::RateLimited));
        for status in [500, 503, 529] {
            assert!(matches!(
                refused(status, json!({})),
                Error::Server { status: s } if s == status
            ));
        }
    }

    #[test]
    fn a_429_for_billing_is_a_refusal() {
        let quota = json!({ "error": { "message": "You exceeded your quota", "type": "insufficient_quota", "code": "insufficient_quota" } });
        let credit =
            json!({ "error": { "message": "No credit", "code": "credit_balance_exhausted" } });
        for body in [quota, credit] {
            let error = refused(429, body);
            assert!(!error.is_transient(), "{error}");
            assert!(matches!(error, Error::Refused { status: 429, .. }));
        }
    }

    #[test]
    fn a_rejected_key_is_never_quoted() {
        let body = json!({ "error": { "message": "Incorrect API key provided: sk-ab***xyz" } });
        for status in [400, 401, 403] {
            let error = refused(status, body.clone());
            assert_eq!(
                error.to_string(),
                format!("the provider refused the request with {status}: the API key was rejected")
            );
        }
    }

    #[test]
    fn other_refusals_quote_the_provider_shortly() {
        let long = "ข".repeat(300);
        let Error::Refused { status, message } =
            refused(400, json!({ "error": { "message": long } }))
        else {
            panic!("not refused");
        };
        assert_eq!(status, 400);
        assert_eq!(message.chars().count(), MESSAGE_CHARS + 1);
        assert!(message.ends_with('…'));

        // xAI may answer with a string; a body that isn't JSON leaves the status
        let xai = refused(422, json!({ "code": "x", "error": "bad field" }));
        assert_eq!(
            xai.to_string(),
            "the provider refused the request with 422: bad field"
        );
        let bare = refusal(StatusCode::NOT_FOUND, None);
        assert_eq!(
            bare.to_string(),
            "the provider refused the request with 404"
        );
    }

    #[test]
    fn paths_are_joined_and_encoded() {
        let http = Http::new(
            Url::parse("https://api.example.com/v1/").unwrap(),
            &Secret::new("key"),
            Auth::Bearer,
            &[],
        )
        .unwrap();
        assert_eq!(
            http.url(&["models", "gemini-3:generateContent"]).as_str(),
            "https://api.example.com/v1/models/gemini-3:generateContent"
        );
        assert_eq!(
            http.url(&["models", "a/../b"]).as_str(),
            "https://api.example.com/v1/models/a%2F..%2Fb"
        );
    }
}
