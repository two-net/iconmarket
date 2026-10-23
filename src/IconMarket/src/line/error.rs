//! What can go wrong talking to LINE, sorted by what the caller does about it.

use std::fmt;

use reqwest::{Response, StatusCode};
use serde::Deserialize;

/// The header LINE puts on every response; quote it when asking LINE about a request.
pub(super) const REQUEST_ID: &str = "x-line-request-id";

/// LINE's `message` for a 429 that means the plan's monthly quota is used up, as opposed to
/// the rate limit.
const MONTHLY_LIMIT: &str = "monthly limit";

/// A request that LINE refused, or never answered.
///
/// No variant holds a URL, a LINE ID, message content or the access token, so errors can be
/// logged as they are (CFG-06, NFR-PRIV-05). `Display` includes the cause.
#[derive(Debug)]
pub enum Error {
    /// The request breaks one of LINE's limits, so it was not sent: a bug in the caller
    /// (FR-LINE-11).
    Limit(LimitError),
    /// No usable answer: a connection failure, a timeout or an unreadable body. The request
    /// may have reached LINE.
    Network(reqwest::Error),
    /// The plan's monthly message quota is used up. Never retried; the caller logs and counts
    /// it (FR-LINE-12, FR-NOT-05).
    Quota { request_id: Option<String> },
    /// The content is larger than the caller accepts.
    TooLarge { max_bytes: usize },
    /// LINE is still preparing the content (a large audio or video file); try again later.
    NotReady,
    /// Any other refusal, e.g. 400 for a used or expired reply token, 404 for an unknown
    /// user, 410 for content the user unsent, or 429 for the rate limit.
    Api {
        status: StatusCode,
        message: String,
        /// `property: message` for each problem LINE found in the request
        details: Vec<String>,
        request_id: Option<String>,
    },
}

/// LINE's error body
#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    message: String,
    #[serde(default)]
    details: Vec<Detail>,
}

#[derive(Deserialize)]
struct Detail {
    message: Option<String>,
    property: Option<String>,
}

impl Error {
    /// The error for an unsuccessful response.
    pub(super) async fn from_response(response: Response) -> Self {
        let status = response.status();
        let request_id = request_id(&response);
        // An unreadable body still leaves the status to go by
        let body = response.json::<Body>().await.unwrap_or(Body {
            message: String::new(),
            details: Vec::new(),
        });
        if status == StatusCode::TOO_MANY_REQUESTS && body.message.contains(MONTHLY_LIMIT) {
            return Self::Quota { request_id };
        }
        let details = body
            .details
            .into_iter()
            .map(|detail| match (detail.property, detail.message) {
                (Some(property), Some(message)) => format!("{property}: {message}"),
                (property, message) => property.or(message).unwrap_or_default(),
            })
            .collect();
        Self::Api {
            status,
            message: body.message,
            details,
            request_id,
        }
    }

    /// The error for a request that got no usable answer. The URL is dropped, since it can
    /// hold a LINE user or message ID.
    pub(super) fn network(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }

    /// Whether a push that failed this way is tried again (FR-LINE-12): after network
    /// errors, 5xx and the rate limit. Retrying any other refusal can't change the answer.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Api { status, .. } => {
                status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS
            }
            Self::Limit(_) | Self::Quota { .. } | Self::TooLarge { .. } | Self::NotReady => false,
        }
    }

    pub(super) fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status, .. } if *status == StatusCode::NOT_FOUND)
    }
}

/// LINE's ID for the request `response` answers.
pub(super) fn request_id(response: &Response) -> Option<String> {
    header(response, REQUEST_ID)
}

pub(super) fn header(response: &Response, name: &str) -> Option<String> {
    let value = response.headers().get(name)?.to_str().ok()?;
    Some(value.to_owned())
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(e) => write!(f, "not sent, it breaks a LINE limit: {e}"),
            Self::Network(e) => {
                f.write_str("no usable answer from LINE")?;
                // reqwest says only "error sending request"; the reason is in its sources
                let mut cause: Option<&dyn std::error::Error> = Some(e);
                while let Some(e) = cause {
                    write!(f, ": {e}")?;
                    cause = e.source();
                }
                Ok(())
            }
            Self::Quota { request_id } => {
                f.write_str("the monthly message quota is used up")?;
                write_request_id(f, request_id.as_deref())
            }
            Self::TooLarge { max_bytes } => {
                write!(f, "the content is larger than {max_bytes} bytes")
            }
            Self::NotReady => f.write_str("LINE is still preparing the content"),
            Self::Api {
                status,
                message,
                details,
                request_id,
            } => {
                write!(f, "LINE answered {status}")?;
                if !message.is_empty() {
                    write!(f, ": {message}")?;
                }
                if !details.is_empty() {
                    write!(f, " ({})", details.join("; "))?;
                }
                write_request_id(f, request_id.as_deref())
            }
        }
    }
}

fn write_request_id(f: &mut fmt::Formatter<'_>, request_id: Option<&str>) -> fmt::Result {
    match request_id {
        Some(id) => write!(f, " [request {id}]"),
        None => Ok(()),
    }
}

impl std::error::Error for Error {}

impl From<LimitError> for Error {
    fn from(e: LimitError) -> Self {
        Self::Limit(e)
    }
}

/// Which limit a request breaks, and where: `messages[2].quickReply.items: 14 items, at most 13`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitError {
    pub path: String,
    pub problem: String,
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.problem)
    }
}

impl std::error::Error for LimitError {}
