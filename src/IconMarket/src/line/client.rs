//! The connection to LINE: TLS, the access token, timeouts and error mapping, plus the calls
//! that read from LINE or only show something: profiles, content and the loading animation.

use std::time::Duration;

use anyhow::Context;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use tracing::debug;
use url::Url;

use super::error::Error;
use crate::config::{LineConfig, Secret};
use crate::tls;

const API: &str = "https://api.line.me/";
/// Content and rich-menu images go through a separate host
const DATA: &str = "https://api-data.line.me/";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(10);
/// For downloads and uploads of up to `image.maxUploadMB`
pub(super) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// Waits before each retry of a push (FR-LINE-12).
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// Content LINE is still preparing is asked for again this many times, this far apart.
const CONTENT_POLLS: u32 = 10;
const CONTENT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The LINE Messaging API client. Cheap to clone: clones share one connection pool.
#[derive(Clone, Debug)]
pub struct Client {
    pub(super) http: reqwest::Client,
    pub(super) api: Url,
    pub(super) data: Url,
    pub(super) backoff: [Duration; 3],
    pub(super) content_poll: Duration,
}

/// A user's LINE profile: only what IconMarket keeps (FR-LINE-10, NFR-PRIV-04).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub display_name: String,
    pub picture_url: Option<String>,
}

/// An image or audio file a user sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    /// As LINE reports it, e.g. `image/jpeg`
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

impl Client {
    pub fn new(config: &LineConfig) -> anyhow::Result<Self> {
        Self::build(
            &config.channel_access_token,
            Url::parse(API)?,
            Url::parse(DATA)?,
            TIMEOUT,
            BACKOFF,
            CONTENT_POLL_INTERVAL,
        )
    }

    /// A client for a fake LINE, which gives up on an answer after a second and waits only
    /// milliseconds between tries.
    #[cfg(test)]
    pub(super) fn for_test(token: &Secret, api: Url, data: Url) -> Self {
        let wait = Duration::from_millis(1);
        Self::build(token, api, data, Duration::from_secs(1), [wait; 3], wait).unwrap()
    }

    fn build(
        token: &Secret,
        api: Url,
        data: Url,
        timeout: Duration,
        backoff: [Duration; 3],
        content_poll: Duration,
    ) -> anyhow::Result<Self> {
        let mut authorization = HeaderValue::try_from(format!("Bearer {}", token.expose()))
            .context("line.channelAccessToken can't be sent in an HTTP header")?;
        // Masked in `Debug` (CFG-06)
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(
                tls::client_config().context("cannot set up TLS for the LINE client")?,
            )
            // Otherwise reqwest reads HTTP_PROXY and friends (CFG-02)
            .no_proxy()
            .default_headers(HeaderMap::from_iter([(AUTHORIZATION, authorization)]))
            .user_agent(concat!("icon-market/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .build()
            .context("cannot set up the LINE client")?;
        Ok(Self {
            http,
            api,
            data,
            backoff,
            content_poll,
        })
    }

    /// `segments` under the API host, each percent-encoded.
    pub(super) fn api_url(&self, segments: &[&str]) -> Url {
        join(&self.api, segments)
    }

    /// `segments` under the content host, each percent-encoded.
    pub(super) fn data_url(&self, segments: &[&str]) -> Url {
        join(&self.data, segments)
    }

    /// Sends `request`, and returns the response if LINE accepted it.
    pub(super) async fn send(&self, request: RequestBuilder) -> Result<Response, Error> {
        let response = request.send().await.map_err(Error::network)?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(Error::from_response(response).await)
        }
    }

    /// Sends `request` and reads LINE's JSON answer.
    pub(super) async fn fetch<T: for<'de> Deserialize<'de>>(
        &self,
        request: RequestBuilder,
    ) -> Result<T, Error> {
        let response = self.send(request).await?;
        response.json().await.map_err(Error::network)
    }

    /// A user's display name and picture (FR-LINE-10). `None` when LINE won't say: the user
    /// isn't a friend, has blocked the OA or hasn't consented.
    pub async fn profile(&self, user_id: &str) -> Result<Option<Profile>, Error> {
        let request = self
            .http
            .get(self.api_url(&["v2", "bot", "profile", user_id]));
        match self.fetch(request).await {
            Ok(profile) => Ok(Some(profile)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Downloads an image or audio file a user sent (FR-LINE-09). LINE keeps it for no
    /// guaranteed time, so call this while processing the event. Content LINE is still
    /// preparing is asked for again for about ten seconds before giving up with
    /// [`Error::NotReady`]; content over `max_bytes` stops with [`Error::TooLarge`].
    ///
    /// Only content with `contentProvider.type` `line` can be downloaded.
    pub async fn content(&self, message_id: &str, max_bytes: usize) -> Result<Content, Error> {
        let url = self.data_url(&["v2", "bot", "message", message_id, "content"]);
        let mut polls = 0;
        let mut response = loop {
            let request = self.http.get(url.clone()).timeout(TRANSFER_TIMEOUT);
            let response = self.send(request).await?;
            if response.status() != StatusCode::ACCEPTED {
                break response;
            }
            if polls == CONTENT_POLLS {
                return Err(Error::NotReady);
            }
            polls += 1;
            tokio::time::sleep(self.content_poll).await;
        };

        let too_large = Error::TooLarge { max_bytes };
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(too_large);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(Error::network)? {
            if bytes.len() + chunk.len() > max_bytes {
                return Err(too_large);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Content {
            content_type,
            bytes,
        })
    }

    /// Shows the loading animation in a user's chat while an AI request runs (FR-LINE-08),
    /// for `seconds` rounded up to LINE's steps of 5, from 5 to 60. It stops early when a
    /// message arrives. Group chats can't show it, so they are skipped.
    pub async fn start_loading(&self, chat_id: &str, seconds: u32) -> Result<(), Error> {
        if !chat_id.starts_with('U') {
            debug!("no loading animation outside one-to-one chats");
            return Ok(());
        }
        let request = self
            .http
            .post(self.api_url(&["v2", "bot", "chat", "loading", "start"]))
            .json(&json!({ "chatId": chat_id, "loadingSeconds": loading_seconds(seconds) }));
        self.send(request).await.map(drop)
    }
}

/// LINE accepts 5 to 60 seconds, in steps of 5.
fn loading_seconds(seconds: u32) -> u32 {
    seconds.div_ceil(5).saturating_mul(5).clamp(5, 60)
}

fn join(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("an http(s) base URL")
        .pop_if_empty()
        .extend(segments);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_lasts_five_to_sixty_seconds_in_steps_of_five() {
        for (asked, shown) in [
            (0, 5),
            (3, 5),
            (5, 5),
            (20, 20),
            (23, 25),
            (60, 60),
            (90, 60),
        ] {
            assert_eq!(loading_seconds(asked), shown, "{asked}");
        }
    }

    #[test]
    fn ids_in_paths_are_encoded() {
        let base = Url::parse(API).unwrap();
        assert_eq!(
            join(&base, &["v2", "bot", "profile", "U1/../x"]).as_str(),
            "https://api.line.me/v2/bot/profile/U1%2F..%2Fx"
        );
    }
}
