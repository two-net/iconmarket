//! Sending messages: reply first, push only when needed (FR-LINE-07), and pushes retried
//! without ever delivering twice (FR-LINE-12).

use reqwest::StatusCode;
use sea_orm::prelude::Uuid;
use serde::Serialize;
use tracing::warn;

use super::client::Client;
use super::error::{self, Error, LimitError};
use super::limits;
use super::message::Message;
use crate::persistence;

/// LINE delivers a push once however often it is sent with the same key, for 24 hours.
const RETRY_KEY: &str = "X-Line-Retry-Key";
/// On a repeated key, the ID of the request LINE accepted before
const ACCEPTED_REQUEST_ID: &str = "x-line-accepted-request-id";

/// Identifies one push across its retries, including retries by a later job: keep it with
/// the push log (FR-NOT-05) to resend the same push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryKey(Uuid);

impl RetryKey {
    /// A new key: a version 4 UUID, as LINE asks for a UUID.
    pub fn random() -> Self {
        let mut bytes = persistence::random_id().into_bytes();
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }

    pub fn uuid(self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for RetryKey {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

/// A push LINE accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pushed {
    /// LINE's ID for the request that delivered it
    pub request_id: Option<String>,
    /// 1 unless it was retried
    pub attempts: u32,
}

/// Whether a response must reach the user when its reply token can't be used (FR-LINE-07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Importance {
    /// Pushed instead, e.g. an order confirmation
    Essential,
    /// Dropped instead, as a push counts against the monthly quota (NFR-COST-01)
    Optional,
}

/// What became of a response.
#[derive(Debug)]
pub enum Delivery {
    /// Sent with the reply token, at no cost
    Replied,
    /// The reply token was missing or failed, so the essential messages were pushed with
    /// `retry_key`. The caller logs the push whatever the result (FR-NOT-05).
    Pushed {
        retry_key: RetryKey,
        result: Result<Pushed, Error>,
    },
    /// The reply token was missing or failed, and the messages weren't essential
    Dropped,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplyRequest<'a> {
    reply_token: &'a str,
    messages: &'a [Message],
}

#[derive(Serialize)]
struct PushRequest<'a, M: Serialize + ?Sized> {
    to: &'a str,
    messages: &'a M,
}

impl Client {
    /// Answers an event with its reply token, which is single-use and lasts about a minute.
    /// Tried once: a reply has no retry key, so a second try could deliver twice.
    pub async fn reply(&self, reply_token: &str, messages: &[Message]) -> Result<(), Error> {
        limits::check(messages)?;
        let request = self
            .http
            .post(self.api_url(&["v2", "bot", "message", "reply"]))
            .json(&ReplyRequest {
                reply_token,
                messages,
            });
        self.send(request).await.map(drop)
    }

    /// Pushes messages to a user or group, which counts against the monthly quota. Network
    /// errors, 5xx and the rate limit are retried up to 3 times with back-off, all with
    /// `retry_key`, so LINE delivers once (FR-LINE-12). The monthly quota is not retried.
    pub async fn push(
        &self,
        to: &str,
        messages: &[Message],
        retry_key: RetryKey,
    ) -> Result<Pushed, Error> {
        limits::check(messages)?;
        self.push_body(&PushRequest { to, messages }, retry_key)
            .await
    }

    /// Pushes messages stored as LINE's JSON, as [`push`](Self::push) does: messages that were
    /// checked against LINE's limits before they were stored, such as a queued notification's.
    pub async fn push_stored(
        &self,
        to: &str,
        messages: &serde_json::Value,
        retry_key: RetryKey,
    ) -> Result<Pushed, Error> {
        self.push_body(&PushRequest { to, messages }, retry_key)
            .await
    }

    /// Pushes `body`, retrying as [`push`](Self::push) describes.
    async fn push_body<M: Serialize + ?Sized>(
        &self,
        body: &PushRequest<'_, M>,
        retry_key: RetryKey,
    ) -> Result<Pushed, Error> {
        let mut waits = self.backoff.iter();
        let mut attempts = 0;
        loop {
            attempts += 1;
            let error = match self.push_once(body, retry_key).await {
                Ok(request_id) => {
                    return Ok(Pushed {
                        request_id,
                        attempts,
                    });
                }
                Err(e) => e,
            };
            let wait = match waits.next() {
                Some(wait) if error.is_retryable() => wait,
                _ => return Err(error),
            };
            warn!(attempts, error = %error, "push failed, retrying with the same retry key");
            tokio::time::sleep(*wait).await;
        }
    }

    /// One try of a push, returning LINE's request ID.
    async fn push_once<M: Serialize + ?Sized>(
        &self,
        body: &PushRequest<'_, M>,
        retry_key: RetryKey,
    ) -> Result<Option<String>, Error> {
        let response = self
            .http
            .post(self.api_url(&["v2", "bot", "message", "push"]))
            .header(RETRY_KEY, retry_key.0.to_string())
            .json(body)
            .send()
            .await
            .map_err(Error::network)?;
        let status = response.status();
        if status.is_success() {
            return Ok(error::request_id(&response));
        }
        // An earlier try was delivered, though its answer was lost
        if status == StatusCode::CONFLICT
            && let Some(accepted) = error::header(&response, ACCEPTED_REQUEST_ID)
        {
            return Ok(Some(accepted));
        }
        Err(Error::from_response(response).await)
    }

    /// Answers a user's event (FR-LINE-07): by reply, else by push when the messages are
    /// essential, else not at all.
    ///
    /// A reply that timed out may still have been delivered, so the push for an essential
    /// message can rarely make it arrive twice; that beats losing an order confirmation.
    pub async fn respond(
        &self,
        reply_token: Option<&str>,
        user_id: &str,
        messages: &[Message],
        importance: Importance,
    ) -> Result<Delivery, LimitError> {
        // A request that breaks a limit would fail as a push too
        limits::check(messages)?;
        if let Some(reply_token) = reply_token {
            match self.reply(reply_token, messages).await {
                Ok(()) => return Ok(Delivery::Replied),
                Err(e) => warn!(error = %e, ?importance, "reply failed"),
            }
        }
        Ok(match importance {
            Importance::Essential => {
                let retry_key = RetryKey::random();
                let result = self.push(user_id, messages, retry_key).await;
                Delivery::Pushed { retry_key, result }
            }
            Importance::Optional => Delivery::Dropped,
        })
    }
}
