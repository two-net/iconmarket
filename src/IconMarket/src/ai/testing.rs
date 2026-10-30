//! Gateways for the tests of features that use the AI: one that is off, and one whose model is
//! the scripted mock, answering as a test says and recording what it was asked.

use std::time::Duration;

use chrono_tz::Asia::Bangkok;
use serde_json::Value;
use sqlx::PgPool;

use super::provider::{Adapter, Capabilities, Error, Mock, Step};
use super::request::{Request, Response, ToolCall, Usage};
use super::{Feature, Gateway};
use crate::config::{AiConfig, AiEndpoint, AiFeatures, AiLimits, AiProvider};
use crate::persistence::testing;

/// Long enough for the mock to answer
const TIMEOUT: Duration = Duration::from_secs(5);

/// What the scripted model does with one request.
pub enum Turn {
    /// Answers with this JSON
    Json(Value),
    /// Answers with this text, calling no tool
    Text(&'static str),
    /// Calls these tools, each by name with its arguments, saying nothing
    Tools(Vec<(&'static str, Value)>),
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
    let capabilities = Capabilities {
        image_input: images,
        ..Capabilities::ALL
    };
    let mock = Mock::scripted(steps(turns)).with_capabilities(capabilities);
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

/// A gateway with every feature on whose model answers with `turns` in turn, and whose fallback
/// (FR-AIP-06) with `fallback_turns`: the gateway and both models, each accepting everything.
/// Its daily limits are counted in `db`.
pub fn scripted_with_fallback(
    db: &PgPool,
    turns: impl IntoIterator<Item = Turn>,
    fallback_turns: impl IntoIterator<Item = Turn>,
) -> (Gateway, Model, Model) {
    let primary = Mock::scripted(steps(turns));
    let fallback = Mock::scripted(steps(fallback_turns)).with_model("fallback-model");
    let gateway = Gateway::with_fallback_for_test(
        &config(),
        Adapter::Mock(primary.clone()),
        Adapter::Mock(fallback.clone()),
        db.clone(),
        TIMEOUT,
    );
    (gateway, Model(primary), Model(fallback))
}

/// What the mock does for each of `turns`
fn steps(turns: impl IntoIterator<Item = Turn>) -> Vec<Step> {
    let reply = |text: String, tool_calls| {
        Step::Reply(Response {
            text,
            tool_calls,
            usage: Usage::default(),
            native: None,
        })
    };
    let mut calls = 0;
    turns
        .into_iter()
        .map(|turn| match turn {
            Turn::Json(value) => reply(value.to_string(), Vec::new()),
            Turn::Text(text) => reply(text.to_owned(), Vec::new()),
            Turn::Tools(called) => {
                let called = called
                    .into_iter()
                    .map(|(name, arguments)| {
                        calls += 1;
                        ToolCall {
                            id: format!("call-{calls}"),
                            name: name.to_owned(),
                            arguments,
                        }
                    })
                    .collect();
                reply(String::new(), called)
            }
            Turn::Fail => Step::Fail(Error::Network("connection refused".to_owned())),
        })
        .collect()
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
