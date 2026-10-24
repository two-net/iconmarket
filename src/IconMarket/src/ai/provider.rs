//! The adapters, one per provider, behind one interface (FR-AIP-02). Adding or changing a
//! provider means a variant of [`Adapter`] and its module here, and nothing outside the
//! gateway (NFR-MNT-02, FR-AIP-03). Each adapter declares what its model accepts from the
//! model's name (FR-AIP-04), and contract tests replay its recorded answers ([`contract`]).

mod claude;
#[cfg(test)]
pub mod contract;
mod dialect;
mod gemini;
mod http;
mod mock;
mod profile;
mod responses;
#[cfg(test)]
pub mod testing;

use std::fmt;

use serde_json::Value;
use url::Url;

use super::call_log::Outcome;
use super::request::{Request, Response};
use crate::config::{AiEndpoint, AiProvider};

pub use claude::Claude;
pub use gemini::Gemini;
pub use mock::Mock;
#[cfg(test)]
pub use mock::Step;
pub use responses::{Flavor, Responses};

/// Why an answer that stopped at the output limit is refused: the same limit would cut it again
const CUT_OFF: &str = "cut off at ai.maxOutputTokens";

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
    Claude(Claude),
    Gemini(Gemini),
    Gpt(Responses),
    Grok(Responses),
    Mock(Mock),
}

impl Adapter {
    /// The adapter for `endpoint`, calling `base_url` instead of the provider's own API when
    /// given (`ai.baseUrl`), or why there is none.
    pub fn new(endpoint: &AiEndpoint, base_url: Option<&Url>) -> Result<Self, String> {
        let model = endpoint.model.as_str();
        let provider = endpoint.provider;
        if provider == AiProvider::Mock {
            return Ok(Self::Mock(Mock::new(model)));
        }
        let key = endpoint
            .api_key
            .as_ref()
            .ok_or_else(|| format!("the {} provider needs ai.apiKey", provider.name()))?;
        Ok(match provider {
            AiProvider::Claude => Self::Claude(Claude::new(model, key, base_url)?),
            AiProvider::Gemini => Self::Gemini(Gemini::new(model, key, base_url)?),
            AiProvider::Gpt => Self::Gpt(Responses::new(Flavor::Gpt, model, key, base_url)?),
            AiProvider::Grok => Self::Grok(Responses::new(Flavor::Grok, model, key, base_url)?),
            AiProvider::Mock => unreachable!("built above"),
        })
    }

    /// As `ai.provider` names it
    pub fn provider(&self) -> &'static str {
        let provider = match self {
            Self::Claude(_) => AiProvider::Claude,
            Self::Gemini(_) => AiProvider::Gemini,
            Self::Gpt(_) => AiProvider::Gpt,
            Self::Grok(_) => AiProvider::Grok,
            Self::Mock(_) => AiProvider::Mock,
        };
        provider.name()
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Claude(claude) => claude.model(),
            Self::Gemini(gemini) => gemini.model(),
            Self::Gpt(responses) | Self::Grok(responses) => responses.model(),
            Self::Mock(mock) => mock.model(),
        }
    }

    /// What the configured model accepts, which may depend on the model (FR-AIP-04)
    pub fn capabilities(&self) -> Capabilities {
        match self {
            Self::Claude(claude) => claude.capabilities(),
            Self::Gemini(gemini) => gemini.capabilities(),
            Self::Gpt(responses) | Self::Grok(responses) => responses.capabilities(),
            Self::Mock(mock) => mock.capabilities(),
        }
    }

    /// Sends `request` once. The gateway applies the timeout and decides about retries.
    pub async fn complete(&self, request: &Request) -> Result<Response, Error> {
        match self {
            Self::Claude(claude) => claude.complete(request).await,
            Self::Gemini(gemini) => gemini.complete(request).await,
            Self::Gpt(responses) | Self::Grok(responses) => responses.complete(request).await,
            Self::Mock(mock) => mock.complete(request).await,
        }
    }
}

/// A token count in a provider's answer; 0 when it didn't say
fn count(value: &Value) -> u32 {
    value
        .as_u64()
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
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
    /// An answer the adapter can't read, or one cut off at the output limit
    Malformed(String),
    /// The model refused, or the provider withheld the answer for safety or policy; why, in
    /// the provider's terms but never its text
    Declined(String),
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
            Self::Refused { .. } | Self::Declined(_) => Outcome::Refused,
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
            Self::Declined(reason) => write!(f, "the model declined to answer: {reason}"),
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
        assert!(!Error::Declined("refusal".to_owned()).is_transient());
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
    fn each_provider_has_its_adapter() {
        let endpoint = |provider| AiEndpoint {
            provider,
            model: "a-model".to_owned(),
            api_key: Some(Secret::new("ai-key-value")),
        };
        for provider in [
            AiProvider::Claude,
            AiProvider::Gemini,
            AiProvider::Gpt,
            AiProvider::Grok,
            AiProvider::Mock,
        ] {
            let adapter = Adapter::new(&endpoint(provider), None).unwrap();
            assert_eq!(
                (adapter.provider(), adapter.model()),
                (provider.name(), "a-model")
            );
        }
        let mock = Adapter::new(
            &AiEndpoint {
                api_key: None,
                ..endpoint(AiProvider::Mock)
            },
            None,
        );
        assert!(mock.is_ok());
    }

    #[test]
    fn a_real_provider_needs_a_key() {
        let endpoint = AiEndpoint {
            provider: AiProvider::Gemini,
            model: "gemini-3.8-flash".to_owned(),
            api_key: None,
        };
        assert_eq!(
            Adapter::new(&endpoint, None).unwrap_err(),
            "the gemini provider needs ai.apiKey"
        );
    }

    /// No model accepts every input: Claude, GPT and Grok take no voice clips (FR-AIP-04)
    #[test]
    fn only_gemini_hears_voice_messages() {
        let audio = |provider, model: &str| {
            let endpoint = AiEndpoint {
                provider,
                model: model.to_owned(),
                api_key: Some(Secret::new("key")),
            };
            Adapter::new(&endpoint, None)
                .unwrap()
                .capabilities()
                .audio_input
        };
        assert!(audio(AiProvider::Gemini, "gemini-3.8-flash"));
        assert!(!audio(AiProvider::Claude, "claude-opus-5"));
        assert!(!audio(AiProvider::Gpt, "gpt-6-astra"));
        assert!(!audio(AiProvider::Grok, "grok-4.7"));
    }
}
