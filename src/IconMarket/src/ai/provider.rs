//! The adapters, one per provider, behind one interface (FR-AIP-02). Adding or changing a
//! provider means a variant of [`Adapter`] and its module here, and nothing outside the
//! gateway (NFR-MNT-02, FR-AIP-03). The Claude, Gemini, GPT and Grok adapters are built by #18.

mod mock;

use std::fmt;

use super::call_log::Outcome;
use super::request::{Request, Response};
use crate::config::{AiEndpoint, AiProvider};

pub use mock::Mock;
#[cfg(test)]
pub use mock::Step;

/// What a provider's model accepts (FR-AIP-04).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub image_input: bool,
    pub audio_input: bool,
    pub tool_calling: bool,
    pub structured_output: bool,
}

impl Capabilities {
    pub const ALL: Self = Self {
        image_input: true,
        audio_input: true,
        tool_calling: true,
        structured_output: true,
    };

    /// What `needed` asks for that these lack, by name; empty when nothing is missing.
    pub fn missing(self, needed: Self) -> Vec<&'static str> {
        [
            (needed.image_input && !self.image_input, "image input"),
            (needed.audio_input && !self.audio_input, "audio input"),
            (needed.tool_calling && !self.tool_calling, "tool calling"),
            (
                needed.structured_output && !self.structured_output,
                "structured output",
            ),
        ]
        .into_iter()
        .filter_map(|(missing, name)| missing.then_some(name))
        .collect()
    }
}

/// The configured provider's adapter.
#[derive(Clone, Debug)]
pub enum Adapter {
    Mock(Mock),
}

impl Adapter {
    /// The adapter for `endpoint`, or why there is none.
    pub fn new(endpoint: &AiEndpoint) -> Result<Self, String> {
        match endpoint.provider {
            AiProvider::Mock => Ok(Self::Mock(Mock::new(&endpoint.model))),
            provider @ (AiProvider::Claude
            | AiProvider::Gemini
            | AiProvider::Gpt
            | AiProvider::Grok) => Err(format!(
                "the {} adapter is not built yet (#18)",
                provider.name()
            )),
        }
    }

    /// As `ai.provider` names it
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Mock(_) => AiProvider::Mock.name(),
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Mock(mock) => mock.model(),
        }
    }

    /// What the configured model accepts, which may depend on the model (FR-AIP-04)
    pub fn capabilities(&self) -> Capabilities {
        match self {
            Self::Mock(mock) => mock.capabilities(),
        }
    }

    /// Sends `request` once. The gateway applies the timeout and decides about retries.
    pub async fn complete(&self, request: &Request) -> Result<Response, Error> {
        match self {
            Self::Mock(mock) => mock.complete(request).await,
        }
    }
}

/// Why a provider gave no response.
///
/// No variant holds the prompt, the response or the API key, so errors can be logged as they
/// are (CFG-06, NFR-PRIV-05).
#[derive(Debug)]
pub enum Error {
    /// 429: too many requests
    RateLimited,
    /// A 5xx, such as an overloaded provider
    Server { status: u16 },
    /// Any other refusal, e.g. 400 for a request the provider rejects or 401 for a bad key
    Refused { status: u16, message: String },
    /// No answer: the connection failed or broke
    Network(String),
    /// An answer the adapter can't read
    Malformed(String),
}

impl Error {
    /// Whether an immediate second try may help (FR-AI-10): a rate limit or a server error
    /// fails fast and often passes the next time. Anything else would fail the same way.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::RateLimited | Self::Server { .. })
    }

    pub(super) fn outcome(&self) -> Outcome {
        match self {
            Self::RateLimited => Outcome::RateLimited,
            Self::Server { .. } => Outcome::ServerError,
            Self::Refused { .. } => Outcome::Refused,
            Self::Network(_) => Outcome::Network,
            Self::Malformed(_) => Outcome::Malformed,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimited => f.write_str("the provider's rate limit was reached"),
            Self::Server { status } => write!(f, "the provider failed with {status}"),
            Self::Refused { status, message } => {
                write!(f, "the provider refused the request with {status}")?;
                if !message.is_empty() {
                    write!(f, ": {message}")?;
                }
                Ok(())
            }
            Self::Network(cause) => write!(f, "no answer from the provider: {cause}"),
            Self::Malformed(cause) => write!(f, "unreadable answer from the provider: {cause}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;

    #[test]
    fn only_rate_limits_and_server_errors_are_transient() {
        assert!(Error::RateLimited.is_transient());
        assert!(Error::Server { status: 503 }.is_transient());
        let refused = Error::Refused {
            status: 400,
            message: String::new(),
        };
        assert!(!refused.is_transient());
        assert!(!Error::Network("reset".to_owned()).is_transient());
        assert!(!Error::Malformed("not JSON".to_owned()).is_transient());
    }

    #[test]
    fn missing_capabilities_are_named() {
        let text_only = Capabilities {
            tool_calling: true,
            ..Capabilities::default()
        };
        assert_eq!(
            text_only.missing(Capabilities::default()),
            Vec::<&str>::new()
        );
        assert_eq!(
            text_only.missing(Capabilities::ALL),
            ["image input", "audio input", "structured output"]
        );
        assert_eq!(
            Capabilities::ALL.missing(Capabilities::ALL),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn real_providers_wait_for_their_adapters() {
        let endpoint = |provider| AiEndpoint {
            provider,
            model: "a-model".to_owned(),
            api_key: Some(Secret::new("ai-key-value")),
        };
        let mock = Adapter::new(&AiEndpoint {
            api_key: None,
            ..endpoint(AiProvider::Mock)
        })
        .unwrap();
        assert_eq!((mock.provider(), mock.model()), ("mock", "a-model"));

        let error = Adapter::new(&endpoint(AiProvider::Claude)).unwrap_err();
        assert_eq!(error, "the claude adapter is not built yet (#18)");
    }
}
