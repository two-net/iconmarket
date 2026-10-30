//! The `gpt` and `grok` providers: the Responses API, which OpenAI and xAI both offer. Their
//! Chat Completions can't call tools on current OpenAI models and drops xAI's reasoning between
//! tool turns.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tracing::warn;
use url::Url;

use super::http::{Auth, Http};
use super::profile::{Profile, Reasoning};
use super::{Capabilities, Error, count, dialect};
use crate::ai::request::{Message, Native, Output, Part, Request, Response, Role, ToolCall, Usage};
use crate::config::{AiProvider, Secret};

/// Constraints strict mode doesn't take
const UNSUPPORTED: [&str; 2] = ["minLength", "maxLength"];
/// Reasons for an incomplete answer that mean it was withheld
const WITHHELD: &str = "content_filter";

/// Which company's API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Gpt,
    Grok,
}

impl Flavor {
    fn provider(self) -> AiProvider {
        match self {
            Self::Gpt => AiProvider::Gpt,
            Self::Grok => AiProvider::Grok,
        }
    }

    pub(super) fn base(self) -> &'static str {
        match self {
            Self::Gpt => "https://api.openai.com/v1/",
            Self::Grok => "https://api.x.ai/v1/",
        }
    }

    fn profile(self, model: &str) -> Profile {
        match self {
            Self::Gpt => gpt(model),
            Self::Grok => grok(model),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Responses {
    flavor: Flavor,
    http: Http,
    model: String,
    profile: Profile,
}

impl Responses {
    pub fn new(
        flavor: Flavor,
        model: &str,
        key: &Secret,
        base_url: Option<&Url>,
    ) -> Result<Self, String> {
        let base = base_url
            .cloned()
            .unwrap_or_else(|| Url::parse(flavor.base()).expect("a URL"));
        Ok(Self {
            flavor,
            http: Http::new(base, key, Auth::Bearer, &[])?,
            model: model.to_owned(),
            profile: flavor.profile(model),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn capabilities(&self) -> Capabilities {
        self.profile.capabilities
    }

    pub async fn complete(&self, request: &Request) -> Result<Response, Error> {
        let url = self.http.url(&["responses"]);
        let body = self.body(request);
        let answer = self.http.post(url, &body, request.timeout).await;
        read(self.flavor.provider().name(), &self.model, answer?)
    }

    fn body(&self, request: &Request) -> Value {
        let provider = self.flavor.provider().name();
        // xAI doesn't count reasoning against the limit
        let limit = match self.flavor {
            Flavor::Gpt => self.profile.output_limit(request.max_output_tokens),
            Flavor::Grok => request.max_output_tokens,
        };
        let mut body = json!({
            "model": self.model,
            "input": input(provider, &self.model, &request.messages),
            "max_output_tokens": limit,
            // Nothing is kept at the provider; the reasoning comes back in the answer instead
            "store": false,
        });
        if !request.system.is_empty() {
            body["instructions"] = json!(request.system);
        }
        if self.profile.temperature {
            body["temperature"] = json!(request.temperature);
        }
        if let Some(Reasoning::Effort(effort)) = self.profile.reasoning {
            body["reasoning"] = json!({ "effort": effort });
        }
        if self.profile.thinks {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        match &request.output {
            Output::Text => {}
            Output::Tools(tools) => {
                let tools: Vec<Value> = tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": schema(tool.parameters.to_json()),
                            "strict": true,
                        })
                    })
                    .collect();
                body["tools"] = json!(tools);
                body["tool_choice"] = json!("auto");
            }
            Output::Json(output) => {
                body["text"] = json!({
                    "format": {
                        "type": "json_schema",
                        "name": "answer",
                        "schema": schema(output.to_json()),
                        "strict": true,
                    },
                });
            }
        }
        body
    }
}

/// A schema as strict mode takes it
fn schema(schema: Value) -> Value {
    dialect::strict(dialect::without(schema, &UNSUPPORTED))
}

/// OpenAI's older models take a temperature and don't reason. The others reason, as little as
/// each allows: replies are short (FR-AI-07) and due within 10 s (NFR-PERF-03); a temperature
/// is only taken when they don't reason at all. GPT models take images, but no voice clip:
/// audio input is limited to audio models and formats LINE doesn't send.
fn gpt(model: &str) -> Profile {
    let capabilities = Capabilities {
        audio_input: false,
        ..Capabilities::ALL
    };
    let classic = ["gpt-4", "gpt-3.5", "chatgpt-", "gpt-5-chat"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    if classic {
        return Profile {
            capabilities,
            temperature: true,
            reasoning: None,
            thinks: false,
        };
    }
    let effort = if model.ends_with("-pro") || model.contains("-pro-") {
        "medium"
    } else if model.starts_with("gpt-5.")
        || model.starts_with("gpt-6-sol")
        || model.starts_with("gpt-6-luna")
    {
        "none"
    } else {
        "low"
    };
    Profile {
        capabilities,
        temperature: effort == "none",
        reasoning: Some(Reasoning::Effort(effort)),
        thinks: effort != "none",
    }
}

/// Grok takes a temperature on every model, and images except on its coding models; no audio
/// in conversation. Reasoning is kept low where the model has a setting for it.
fn grok(model: &str) -> Profile {
    let coding = model.starts_with("grok-build") || model.starts_with("grok-code");
    let capabilities = Capabilities {
        image_input: !coding,
        audio_input: false,
        ..Capabilities::ALL
    };
    let effort = if model.starts_with("grok-4.3") {
        Some("none")
    } else if ["grok-4.5", "grok-4.6", "grok-4.7"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        Some("low")
    } else {
        None
    };
    Profile {
        capabilities,
        temperature: true,
        reasoning: effort.map(Reasoning::Effort),
        thinks: effort != Some("none") && !model.contains("non-reasoning"),
    }
}

/// The conversation as input items.
fn input(provider: &str, model: &str, messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        if let Some(own) = own_turn(provider, model, message) {
            items.extend(own.iter().cloned());
            continue;
        }
        match message.role {
            Role::User | Role::Tool => {
                let mut content = Vec::new();
                for part in &message.parts {
                    match part {
                        Part::Text(text) if !text.is_empty() => {
                            content.push(json!({ "type": "input_text", "text": text }));
                        }
                        Part::Image(media) => content.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", media.media_type, STANDARD.encode(&media.bytes)),
                        })),
                        Part::Audio(media) => {
                            // The gateway never asks a model without audio (FR-AIP-04)
                            warn!(media = ?media, provider, "no audio input; the clip is left out");
                        }
                        Part::ToolResult { call_id, content } => items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": as_text(content),
                        })),
                        Part::Text(_) | Part::ToolCall(_) | Part::Native(_) => {}
                    }
                }
                if !content.is_empty() {
                    items.push(json!({ "role": "user", "content": content }));
                }
            }
            Role::Assistant => {
                let text = message.text();
                if !text.is_empty() {
                    items.push(json!({ "role": "assistant", "content": text }));
                }
                for part in &message.parts {
                    if let Part::ToolCall(call) = part {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": as_text(&call.arguments),
                        }));
                    }
                }
            }
        }
    }
    items
}

/// The output items the provider sent for this turn, when it was this model's; they keep its
/// reasoning.
fn own_turn<'a>(provider: &str, model: &str, message: &'a Message) -> Option<&'a Vec<Value>> {
    message.parts.iter().find_map(|part| match part {
        Part::Native(native) if native.provider == provider && native.model == model => {
            native.content.as_array()
        }
        _ => None,
    })
}

/// A string as itself, anything else as JSON
fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}

fn read(provider: &'static str, model: &str, answer: Value) -> Result<Response, Error> {
    match answer["status"].as_str() {
        Some("incomplete") => {
            let reason = answer
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("unspecified");
            return Err(match reason {
                "max_output_tokens" => Error::Malformed(super::CUT_OFF.to_owned()),
                WITHHELD => Error::Declined(format!("withheld ({reason})")),
                reason => Error::Malformed(format!("the answer is incomplete ({reason})")),
            });
        }
        Some("failed") => return Err(Error::Malformed("the response failed".to_owned())),
        _ => {}
    }
    let Some(output) = answer["output"].as_array() else {
        return Err(Error::Malformed("no output".to_owned()));
    };

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for content in item["content"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    match content["type"].as_str() {
                        Some("output_text") => {
                            text.push_str(content["text"].as_str().unwrap_or_default());
                        }
                        // The refusal's text is the model's, so it isn't kept
                        Some("refusal") => return Err(Error::Declined("refusal".to_owned())),
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let arguments = item["arguments"].as_str().unwrap_or_default();
                tool_calls.push(ToolCall {
                    id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                    name: item["name"].as_str().unwrap_or_default().to_owned(),
                    // Kept as written when it isn't JSON, so validation names the problem
                    arguments: serde_json::from_str(arguments)
                        .unwrap_or_else(|_| Value::String(arguments.to_owned())),
                });
            }
            // Reasoning stays in the native turn
            _ => {}
        }
    }
    let usage = &answer["usage"];
    let input_tokens = count(&usage["input_tokens"]);
    // xAI may leave reasoning out of `output_tokens`; the total always has it
    let output_tokens = match count(&usage["total_tokens"]).checked_sub(input_tokens) {
        Some(output) if output > 0 => output,
        _ => count(&usage["output_tokens"]),
    };
    Ok(Response {
        text,
        tool_calls,
        usage: Usage {
            input_tokens,
            output_tokens,
        },
        native: Some(Native {
            provider,
            model: model.to_owned(),
            content: Value::Array(output.clone()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::profile::THINKING_ALLOWANCE;
    use super::*;
    use crate::ai::request::{Media, Tool};
    use crate::ai::schema::Schema;

    fn adapter(flavor: Flavor, model: &str) -> Responses {
        Responses::new(flavor, model, &Secret::new("key"), None).unwrap()
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

    fn knobs(flavor: Flavor, model: &str) -> Value {
        let body = adapter(flavor, model).body(&request(vec![Message::user("x")], Output::Text));
        json!({
            "temperature": body.get("temperature"),
            "reasoning": body.get("reasoning"),
            "include": body.get("include"),
            "max_output_tokens": body["max_output_tokens"],
        })
    }

    #[test]
    fn gpt_reasons_as_little_as_each_model_allows() {
        let allowance = 1024 + THINKING_ALLOWANCE;
        assert_eq!(
            knobs(Flavor::Gpt, "gpt-4.1"),
            json!({ "temperature": 0.3, "reasoning": null, "include": null, "max_output_tokens": 1024 })
        );
        for model in ["gpt-5.5", "gpt-6-luna", "gpt-6-sol"] {
            assert_eq!(
                knobs(Flavor::Gpt, model),
                json!({ "temperature": 0.3, "reasoning": { "effort": "none" }, "include": null, "max_output_tokens": 1024 }),
                "{model}"
            );
        }
        for (model, effort) in [
            ("gpt-6-astra", "low"),
            ("gpt-6.1-sol", "low"),
            ("gpt-5.5-pro", "medium"),
            ("gpt-next", "low"),
        ] {
            assert_eq!(
                knobs(Flavor::Gpt, model),
                json!({ "temperature": null, "reasoning": { "effort": effort }, "include": ["reasoning.encrypted_content"], "max_output_tokens": allowance }),
                "{model}"
            );
        }
        assert!(
            !adapter(Flavor::Gpt, "gpt-6-astra")
                .capabilities()
                .audio_input
        );
    }

    #[test]
    fn grok_keeps_its_temperature_and_its_limit() {
        assert_eq!(
            knobs(Flavor::Grok, "grok-4.7"),
            json!({ "temperature": 0.3, "reasoning": { "effort": "low" }, "include": ["reasoning.encrypted_content"], "max_output_tokens": 1024 })
        );
        assert_eq!(
            knobs(Flavor::Grok, "grok-4.3"),
            json!({ "temperature": 0.3, "reasoning": { "effort": "none" }, "include": null, "max_output_tokens": 1024 })
        );
        assert_eq!(
            knobs(Flavor::Grok, "grok-4.20-0309-non-reasoning")["include"],
            Value::Null
        );
        assert!(adapter(Flavor::Grok, "grok-4.7").capabilities().image_input);
        assert!(
            !adapter(Flavor::Grok, "grok-build-0.1")
                .capabilities()
                .image_input
        );
    }

    #[test]
    fn the_conversation_is_a_flat_list_of_items() {
        let call = ToolCall {
            id: "call_1".to_owned(),
            name: "get_cart".to_owned(),
            arguments: json!({}),
        };
        let messages = vec![
            Message {
                role: Role::User,
                parts: vec![
                    Part::Text("รูปนี้คืออะไร".to_owned()),
                    Part::Image(Media {
                        media_type: "image/jpeg".to_owned(),
                        bytes: vec![1, 2, 3],
                    }),
                ],
            },
            Message {
                role: Role::Assistant,
                parts: vec![Part::Text("ดูให้นะคะ".to_owned()), Part::ToolCall(call)],
            },
            Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    call_id: "call_1".to_owned(),
                    content: json!({ "items": 2 }),
                }],
            },
        ];
        let body = adapter(Flavor::Gpt, "gpt-6-astra").body(&request(messages, Output::Text));
        assert_eq!(
            body["input"],
            json!([
                { "role": "user", "content": [
                    { "type": "input_text", "text": "รูปนี้คืออะไร" },
                    { "type": "input_image", "image_url": "data:image/jpeg;base64,AQID" },
                ] },
                { "role": "assistant", "content": "ดูให้นะคะ" },
                { "type": "function_call", "call_id": "call_1", "name": "get_cart", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "{\"items\":2}" },
            ])
        );
        assert_eq!(
            (body["instructions"].as_str(), body["store"].as_bool()),
            (Some("กติกา"), Some(false))
        );
    }

    #[test]
    fn its_own_turn_goes_back_unchanged() {
        let output = json!([
            { "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc" },
            { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_cart", "arguments": "{}" },
        ]);
        let turn = |provider, model: &str| Message {
            role: Role::Assistant,
            parts: vec![Part::Native(Native {
                provider,
                model: model.to_owned(),
                content: output.clone(),
            })],
        };
        let sent = |flavor, turn| {
            adapter(flavor, "m").body(&request(vec![turn], Output::Text))["input"].clone()
        };
        assert_eq!(sent(Flavor::Gpt, turn("gpt", "m")), output);
        // Grok speaks the same API, but its turn isn't GPT's
        assert_eq!(sent(Flavor::Grok, turn("gpt", "m")), json!([]));
    }

    #[test]
    fn schemas_are_sent_in_strict_form() {
        let draft = Schema::object()
            .property("name", Schema::string().max_length(10))
            .optional("stock", Schema::integer().minimum(1));
        let body = adapter(Flavor::Gpt, "gpt-6-astra").body(&request(
            vec![Message::user("x")],
            Output::Json(draft.clone()),
        ));
        assert_eq!(
            body["text"],
            json!({ "format": { "type": "json_schema", "name": "answer", "strict": true, "schema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "stock": { "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "null" }] },
                },
                "required": ["name", "stock"],
                "additionalProperties": false,
            } } })
        );
        let tool = Tool {
            name: "add".to_owned(),
            description: "Add".to_owned(),
            parameters: draft,
        };
        let body = adapter(Flavor::Grok, "grok-4.7").body(&request(
            vec![Message::user("x")],
            Output::Tools(vec![tool]),
        ));
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(
            body["tools"][0]["parameters"]["required"],
            json!(["name", "stock"])
        );
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn answers_are_read_from_the_output_items() {
        let answer = json!({
            "status": "completed",
            "output": [
                { "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc" },
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "ดูให้นะคะ" }] },
                { "type": "function_call", "call_id": "call_1", "name": "get_cart", "arguments": "{\"a\":1}" },
                { "type": "function_call", "call_id": "call_2", "name": "get_cart", "arguments": "{broken" },
            ],
            "usage": { "input_tokens": 100, "output_tokens": 9, "total_tokens": 203 },
        });
        let response = read("grok", "grok-4.7", answer.clone()).unwrap();
        assert_eq!(response.text, "ดูให้นะคะ");
        assert_eq!(response.tool_calls[0].arguments, json!({ "a": 1 }));
        assert_eq!(response.tool_calls[1].arguments, json!("{broken"));
        // Reasoning counted once, whether or not `output_tokens` has it
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 103
            }
        );
        assert_eq!(response.native.unwrap().content, answer["output"]);
    }

    #[test]
    fn refusals_and_incomplete_answers_are_errors() {
        let refusal = json!({ "status": "completed", "output": [
            { "type": "message", "content": [{ "type": "refusal", "refusal": "ขอโทษค่ะ" }] },
        ] });
        let error = read("gpt", "m", refusal).unwrap_err();
        assert_eq!(error.to_string(), "the model declined to answer: refusal");
        let incomplete = |reason: &str| {
            read(
                "gpt",
                "m",
                json!({ "status": "incomplete", "incomplete_details": { "reason": reason }, "output": [] }),
            )
        };
        assert!(matches!(
            incomplete("max_output_tokens"),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            incomplete("content_filter"),
            Err(Error::Declined(_))
        ));
    }
}
