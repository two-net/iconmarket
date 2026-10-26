//! Gateways for the tests of features that use the AI: one that is off, and one whose model is
//! the scripted mock, answering as a test says and recording what it was asked.

use std::time::Duration;

use chrono_tz::Asia::Bangkok;
use serde_json::Value;
use sqlx::PgPool;

use super::provider::{Adapter, Capabilities, Error, Mock, Step};
use super::request::{Request, Response, Usage};
use super::{Feature, Gateway};
use crate::config::{AiConfig, AiEndpoint, AiFeatures, AiLimits, AiProvider};
use crate::persistence::testing;

/// Long enough for the mock to answer
const TIMEOUT: Duration = Duration::from_secs(5);

/// What the scripted model does with one request.
pub enum Turn {
    /// Answers with this JSON
    Json(Value),
    /// Fails as an unreachable provider does, which isn't tried again
    Fail,
}

/// The scripted mock behind a [`scripted`] gateway.
#[derive(Clone)]
pub struct Model(Mock);

impl Model {
    /// Every request the model received, in order
    pub fn requests(&self) -> Vec<Request> {
        self.0.requests()
    }
}

/// A gateway with the AI off, as `ai.enabled = false` makes it (FR-AI-10).
pub fn off() -> Gateway {
    let config = AiConfig {
        primary: None,
        ..config()
    };
    Gateway::new(&config, Bangkok, testing::unreachable())
}

/// A gateway with every feature on but those in `off`, whose model answers with `turns` in
/// turn and accepts images if `images`. Its daily limits are counted in `db`.
pub fn scripted(
    db: &PgPool,
    turns: impl IntoIterator<Item = Turn>,
    images: bool,
    off: &[Feature],
) -> (Gateway, Model) {
    let steps = turns.into_iter().map(|turn| match turn {
        Turn::Json(value) => Step::Reply(Response {
            text: value.to_string(),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            native: None,
        }),
        Turn::Fail => Step::Fail(Error::Network("connection refused".to_owned())),
    });
    let capabilities = Capabilities {
        image_input: images,
        ..Capabilities::ALL
    };
    let mock = Mock::scripted(steps).with_capabilities(capabilities);
    let mut config = config();
    for feature in off {
        match feature {
            Feature::Assistant => config.features.assistant = false,
            Feature::Listing => config.features.listing = false,
            Feature::Moderation => config.features.moderation = false,
            Feature::SlipReading => config.features.slip_reading = false,
            Feature::Voice => config.features.voice = false,
            Feature::VisualSearch => config.features.visual_search = false,
        }
    }
    let adapter = Adapter::Mock(mock.clone());
    let gateway = Gateway::for_test(&config, Some(adapter), db.clone(), TIMEOUT);
    (gateway, Model(mock))
}

/// The mock provider with every feature on, and limits no test reaches.
fn config() -> AiConfig {
    AiConfig {
        primary: Some(AiEndpoint {
            provider: AiProvider::Mock,
            model: "mock-model".to_owned(),
            api_key: None,
        }),
        base_url: None,
        timeout_seconds: 20,
        max_output_tokens: 1024,
        temperature: 0.3,
        history_turns: 10,
        features: AiFeatures {
            assistant: true,
            listing: true,
            moderation: true,
            slip_reading: true,
            voice: true,
            visual_search: true,
        },
        limits: AiLimits {
            per_user_daily_calls: 1_000,
            global_daily_calls: 10_000,
        },
        log_content: false,
        fallback: None,
    }
}
