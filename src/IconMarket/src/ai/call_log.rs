//! Logging every AI call (FR-AI-16, NFR-COST-02): a row in `ai_call_logs` for usage and cost,
//! and a log line. Prompt and response content is logged only when `ai.logContent` is on
//! (NFR-PRIV-05).

use std::time::Duration;

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{DatabaseConnection, EntityTrait};
use tracing::{info, warn};

use super::Feature;
use super::request::{Message, Part, Request, Response, Role, ToolCall, Usage};
use crate::persistence::entity::ai_call_log;

/// How a call ended, as stored in `ai_call_logs.outcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The answer failed validation (FR-AI-05)
    Invalid,
    Timeout,
    RateLimited,
    ServerError,
    Refused,
    Network,
    Malformed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Invalid => "invalid",
            Self::Timeout => "timeout",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::Refused => "refused",
            Self::Network => "network",
            Self::Malformed => "malformed",
        }
    }
}

/// One call to the provider.
pub struct Call<'a> {
    pub feature: Feature,
    pub user_id: Option<i64>,
    pub provider: &'static str,
    pub model: &'a str,
    pub usage: Usage,
    pub latency: Duration,
    pub outcome: Outcome,
    /// The error, or what was invalid; never content
    pub detail: Option<String>,
}

/// Stores `call` and logs it. A failure to store is logged and otherwise ignored, so logging
/// never costs the user an answer.
pub async fn record(db: &DatabaseConnection, call: Call<'_>) {
    let latency_ms = i32::try_from(call.latency.as_millis()).unwrap_or(i32::MAX);
    let (feature, outcome) = (call.feature.name(), call.outcome.as_str());
    let (input_tokens, output_tokens) = (call.usage.input_tokens, call.usage.output_tokens);
    match &call.detail {
        None => info!(
            feature,
            provider = call.provider,
            model = call.model,
            input_tokens,
            output_tokens,
            latency_ms,
            outcome,
            "AI call"
        ),
        Some(detail) => warn!(
            feature,
            provider = call.provider,
            model = call.model,
            input_tokens,
            output_tokens,
            latency_ms,
            outcome,
            detail = %detail,
            "AI call failed"
        ),
    }

    let row = ai_call_log::ActiveModel {
        user_id: Set(call.user_id),
        feature: Set(feature.to_owned()),
        provider: Set(call.provider.to_owned()),
        model: Set(call.model.to_owned()),
        input_tokens: Set(i32::try_from(input_tokens).unwrap_or(i32::MAX)),
        output_tokens: Set(i32::try_from(output_tokens).unwrap_or(i32::MAX)),
        latency_ms: Set(latency_ms),
        outcome: Set(outcome.to_owned()),
        created_at: Set(Utc::now()),
        ..Default::default()
    };
    if let Err(e) = ai_call_log::Entity::insert(row)
        .exec_without_returning(db)
        .await
    {
        warn!(error = %e, feature, outcome, "cannot store the AI call log");
    }
}

/// Logs what was sent and what came back; call it only when `ai.logContent` is on.
pub fn content(feature: Feature, request: &Request, response: Option<&Response>) {
    info!(
        feature = feature.name(),
        system = %request.system,
        messages = transcript(&request.messages),
        response = response.map(|response| response.text.as_str()),
        tool_calls = response.map(|response| calls(&response.tool_calls)),
        "AI call content"
    );
}

/// The conversation as readable lines, `user: text`, with images and audio as their type and
/// size. `Debug` would escape Thai vowel and tone marks.
fn transcript(messages: &[Message]) -> String {
    let lines: Vec<String> = messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let parts: Vec<String> = message
                .parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => text.clone(),
                    Part::Image(media) | Part::Audio(media) => format!("{media:?}"),
                    Part::ToolCall(call) => calls(std::slice::from_ref(call)),
                    Part::ToolResult { call_id, content } => format!("[{call_id}] {content}"),
                })
                .collect();
            format!("{role}: {}", parts.join(" "))
        })
        .collect();
    lines.join("\n")
}

/// `name(arguments) [id]`, separated by `; `
fn calls(calls: &[ToolCall]) -> String {
    let calls: Vec<String> = calls
        .iter()
        .map(|call| format!("{}({}) [{}]", call.name, call.arguments, call.id))
        .collect();
    calls.join("; ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ai::request::Media;

    #[test]
    fn the_transcript_keeps_thai_readable_and_media_out() {
        let messages = [
            Message {
                role: Role::User,
                parts: vec![
                    Part::Text("มีแบบนี้ไหม".to_owned()),
                    Part::Image(Media {
                        media_type: "image/jpeg".to_owned(),
                        bytes: vec![0xff; 2048],
                    }),
                ],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall(ToolCall {
                    id: "c1".to_owned(),
                    name: "search_products".to_owned(),
                    arguments: json!({ "keywords": "ไข่ไก่" }),
                })],
            },
            Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    call_id: "c1".to_owned(),
                    content: json!({ "count": 2 }),
                }],
            },
        ];
        assert_eq!(
            transcript(&messages),
            "user: มีแบบนี้ไหม <image/jpeg, 2048 bytes>\n\
             assistant: search_products({\"keywords\":\"ไข่ไก่\"}) [c1]\n\
             tool: [c1] {\"count\":2}"
        );
    }
}
