//! The `claude` provider: Anthropic's Messages API.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tracing::warn;
use url::Url;

use super::http::{Auth, Http};
use super::profile::{self, Profile, Reasoning};
use super::{Capabilities, Error, count, dialect};
use crate::ai::request::{Message, Native, Output, Part, Request, Response, Role, ToolCall, Usage};
use crate::config::{AiProvider, Secret};

pub(super) const BASE: &str = "https://api.anthropic.com/";
const VERSION: &str = "2023-06-01";
/// Constraints structured outputs and strict tools don't take
const UNSUPPORTED: [&str; 4] = ["minimum", "maximum", "minLength", "maxLength"];

#[derive(Clone, Debug)]
pub struct Claude {
    http: Http,
    model: String,
    profile: Profile,
}

impl Claude {
    pub fn new(model: &str, key: &Secret, base_url: Option<&Url>) -> Result<Self, String> {
        let base = base_url
            .cloned()
            .unwrap_or_else(|| Url::parse(BASE).expect("a URL"));
        let headers = [("anthropic-version", VERSION)];
        Ok(Self {
            http: Http::new(base, key, Auth::Header("x-api-key"), &headers)?,
            model: model.to_owned(),
            profile: profile(model),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn capabilities(&self) -> Capabilities {
        self.profile.capabilities
    }

    pub async fn complete(&self, request: &Request) -> Result<Response, Error> {
        let url = self.http.url(&["v1", "messages"]);
        let body = self.body(request);
        let answer = self.http.post(url, &body, request.timeout).await;
        read(&self.model, answer?)
    }

    fn body(&self, request: &Request) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.profile.output_limit(request.max_output_tokens),
            "messages": messages(&self.model, &request.messages),
        });
        if !request.system.is_empty() {
            body["system"] = json!(request.system);
        }
        if self.profile.temperature {
            body["temperature"] = json!(request.temperature);
        }
        if let Some(Reasoning::Effort(effort)) = self.profile.reasoning {
            body["output_config"]["effort"] = json!(effort);
        }
        match &request.output {
            Output::Text => {}
            Output::Tools(tools) => {
                let tools: Vec<Value> = tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": dialect::without(tool.parameters.to_json(), &UNSUPPORTED),
                            "strict": true,
                        })
                    })
                    .collect();
                body["tools"] = json!(tools);
                body["tool_choice"] = json!({ "type": "auto" });
            }
            Output::Json(schema) => {
                body["output_config"]["format"] = json!({
                    "type": "json_schema",
                    "schema": dialect::without(schema.to_json(), &UNSUPPORTED),
                });
            }
        }
        body
    }
}

/// Models before 4.7 take a temperature and don't think unless asked. Later ones reject
/// sampling parameters with a 400, and are asked for little effort: replies are short
/// (FR-AI-07) and due within 10 s (NFR-PERF-03). No Claude model accepts audio.
fn profile(model: &str) -> Profile {
    let capabilities = Capabilities {
        audio_input: false,
        ..Capabilities::ALL
    };
    let older = profile::version(model).is_some_and(|version| version < (4, 7));
    Profile {
        capabilities,
        temperature: older,
        reasoning: (!older).then_some(Reasoning::Effort("low")),
        thinks: !older,
    }
}

/// The conversation as Claude's turns. Tool results go in a `user` turn, and consecutive
/// turns of one role are merged.
fn messages(model: &str, messages: &[Message]) -> Vec<Value> {
    let mut turns: Vec<(&str, Vec<Value>)> = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
        };
        let blocks = match own_turn(model, message) {
            Some(content) => content.clone(),
            None => message.parts.iter().filter_map(block).collect(),
        };
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

/// The blocks Claude sent for this turn, when it was this model's; they keep its thinking.
fn own_turn<'a>(model: &str, message: &'a Message) -> Option<&'a Vec<Value>> {
    message.parts.iter().find_map(|part| match part {
        Part::Native(native)
            if native.provider == AiProvider::Claude.name() && native.model == model =>
        {
            native.content.as_array()
        }
        _ => None,
    })
}

fn block(part: &Part) -> Option<Value> {
    match part {
        Part::Text(text) if text.is_empty() => None,
        Part::Text(text) => Some(json!({ "type": "text", "text": text })),
        Part::Image(media) => Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media.media_type,
                "data": STANDARD.encode(&media.bytes),
            },
        })),
        Part::Audio(media) => {
            // The gateway turns off the features that send audio (FR-AIP-04)
            warn!(media = ?media, "Claude takes no audio; the clip is left out");
            None
        }
        Part::ToolCall(call) => Some(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            // Arguments that weren't an object were rejected; Claude needs one back
            "input": if call.arguments.is_object() { call.arguments.clone() } else { json!({}) },
        })),
        Part::ToolResult { call_id, content } => Some(json!({
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": as_text(content),
        })),
        // Another provider's turn; this one is rebuilt from the other parts
        Part::Native(_) => None,
    }
}

/// A string as itself, anything else as JSON
fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}

fn read(model: &str, answer: Value) -> Result<Response, Error> {
    match answer["stop_reason"].as_str() {
        Some("refusal") => {
            let category = answer
                .pointer("/stop_details/category")
                .and_then(Value::as_str)
                .unwrap_or("unspecified");
            return Err(Error::Declined(format!("refusal ({category})")));
        }
        Some("max_tokens") => return Err(Error::Malformed(super::CUT_OFF.to_owned())),
        _ => {}
    }
    let Some(content) = answer["content"].as_array() else {
        return Err(Error::Malformed("no content".to_owned()));
    };

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in content {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => tool_calls.push(ToolCall {
                id: string(&block["id"]),
                name: string(&block["name"]),
                arguments: block["input"].clone(),
            }),
            // Thinking stays in the native turn
            _ => {}
        }
    }
    let usage = &answer["usage"];
    let input = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|name| count(&usage[name]))
    .fold(0, u32::saturating_add);
    Ok(Response {
        text,
        tool_calls,
        usage: Usage {
            input_tokens: input,
            output_tokens: count(&usage["output_tokens"]),
        },
        native: Some(Native {
            provider: AiProvider::Claude.name(),
            model: model.to_owned(),
            content: Value::Array(content.clone()),
        }),
    })
}

fn string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ai::request::{Media, Tool};
    use crate::ai::schema::Schema;

    fn claude(model: &str) -> Claude {
        Claude::new(model, &Secret::new("key"), None).unwrap()
    }

    fn request(messages: Vec<Message>, output: Output) -> Request {
        Request {
            system: "กติกา".to_owned(),
            messages,
            output,
            max_output_tokens: 1024,
            temperature: 0.3,
            timeout: Duration::from_secs(20),
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "toolu_1".to_owned(),
            name: "get_cart".to_owned(),
            arguments: json!({}),
        }
    }

    #[test]
    fn older_models_take_a_temperature_and_newer_ones_little_effort() {
        for model in [
            "claude-3-5-haiku-latest",
            "claude-sonnet-4-20250514",
            "claude-haiku-4-5",
            "claude-opus-4-6",
        ] {
            let body = claude(model).body(&request(vec![Message::user("x")], Output::Text));
            assert_eq!(body["temperature"], 0.3, "{model}");
            assert_eq!(body["max_tokens"], 1024, "{model}");
            assert!(body.get("output_config").is_none(), "{model}");
        }
        for model in [
            "claude-opus-4-7",
            "claude-opus-5",
            "claude-fable-5",
            "claude-next",
        ] {
            let body = claude(model).body(&request(vec![Message::user("x")], Output::Text));
            assert!(body.get("temperature").is_none(), "{model}");
            assert_eq!(body["output_config"], json!({ "effort": "low" }), "{model}");
            assert_eq!(
                body["max_tokens"],
                1024 + profile::THINKING_ALLOWANCE,
                "{model}"
            );
        }
        assert!(!claude("claude-opus-5").capabilities().audio_input);
        assert!(claude("claude-opus-5").capabilities().image_input);
    }

    #[test]
    fn tool_results_join_the_users_turn_after_the_call() {
        let messages = vec![
            Message::user("ตะกร้ามีอะไร"),
            Message {
                role: Role::Assistant,
                parts: vec![Part::Text("ดูให้นะคะ".to_owned()), Part::ToolCall(call())],
            },
            Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    call_id: "toolu_1".to_owned(),
                    content: json!({ "items": 2 }),
                }],
            },
            Message {
                role: Role::User,
                parts: vec![
                    Part::Text("แล้วรูปนี้ล่ะ".to_owned()),
                    Part::Image(Media {
                        media_type: "image/png".to_owned(),
                        bytes: vec![1, 2, 3],
                    }),
                ],
            },
        ];
        let body = claude("claude-opus-5").body(&request(messages, Output::Text));
        assert_eq!(
            body["messages"],
            json!([
                { "role": "user", "content": [{ "type": "text", "text": "ตะกร้ามีอะไร" }] },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "ดูให้นะคะ" },
                    { "type": "tool_use", "id": "toolu_1", "name": "get_cart", "input": {} },
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"items\":2}" },
                    { "type": "text", "text": "แล้วรูปนี้ล่ะ" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AQID" } },
                ] },
            ])
        );
        assert_eq!(body["system"], "กติกา");
    }

    #[test]
    fn its_own_turn_goes_back_unchanged_and_others_are_rebuilt() {
        let content = json!([
            { "type": "thinking", "thinking": "", "signature": "sig" },
            { "type": "tool_use", "id": "toolu_1", "name": "get_cart", "input": {} },
        ]);
        let turn = |provider, model: &str| Message {
            role: Role::Assistant,
            parts: vec![
                Part::ToolCall(call()),
                Part::Native(Native {
                    provider,
                    model: model.to_owned(),
                    content: content.clone(),
                }),
            ],
        };
        let sent = |turn| {
            let body = claude("claude-opus-5").body(&request(vec![turn], Output::Text));
            body["messages"][0]["content"].clone()
        };
        assert_eq!(sent(turn("claude", "claude-opus-5")), content);
        let rebuilt =
            json!([{ "type": "tool_use", "id": "toolu_1", "name": "get_cart", "input": {} }]);
        assert_eq!(sent(turn("claude", "claude-sonnet-5")), rebuilt);
        assert_eq!(sent(turn("gemini", "claude-opus-5")), rebuilt);
    }

    #[test]
    fn schemas_lose_the_constraints_claude_does_not_take() {
        let schema = Schema::object().property("qty", Schema::integer().minimum(1));
        let body = claude("claude-opus-5").body(&request(
            vec![Message::user("x")],
            Output::Json(schema.clone()),
        ));
        assert_eq!(
            body["output_config"],
            json!({
                "effort": "low",
                "format": { "type": "json_schema", "schema": {
                    "type": "object",
                    "properties": { "qty": { "type": "integer" } },
                    "required": ["qty"],
                    "additionalProperties": false,
                } },
            })
        );

        let tool = Tool {
            name: "set_stock".to_owned(),
            description: "Stock".to_owned(),
            parameters: schema,
        };
        let body = claude("claude-opus-5").body(&request(
            vec![Message::user("x")],
            Output::Tools(vec![tool]),
        ));
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["qty"],
            json!({ "type": "integer" })
        );
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tool_choice"], json!({ "type": "auto" }));
    }

    #[test]
    fn answers_are_read_with_their_native_turn() {
        let answer = json!({
            "content": [
                { "type": "thinking", "thinking": "", "signature": "sig" },
                { "type": "text", "text": "ดูให้นะคะ" },
                { "type": "tool_use", "id": "toolu_1", "name": "get_cart", "input": {} },
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 100, "cache_read_input_tokens": 20, "output_tokens": 30 },
        });
        let response = read("claude-opus-5", answer.clone()).unwrap();
        assert_eq!(response.text, "ดูให้นะคะ");
        assert_eq!(response.tool_calls, [call()]);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 120,
                output_tokens: 30
            }
        );
        let native = response.native.unwrap();
        assert_eq!(
            (native.provider, native.model.as_str()),
            ("claude", "claude-opus-5")
        );
        assert_eq!(native.content, answer["content"]);
    }

    #[test]
    fn refusals_and_cut_off_answers_are_errors() {
        let refusal = json!({
            "content": [],
            "stop_reason": "refusal",
            "stop_details": { "type": "refusal", "category": "cyber", "explanation": "…" },
        });
        let error = read("m", refusal).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the model declined to answer: refusal (cyber)"
        );
        assert!(!error.is_transient());

        let cut =
            json!({ "content": [{ "type": "text", "text": "ตล" }], "stop_reason": "max_tokens" });
        assert!(matches!(read("m", cut), Err(Error::Malformed(_))));
        assert!(matches!(read("m", json!({})), Err(Error::Malformed(_))));
    }
}
