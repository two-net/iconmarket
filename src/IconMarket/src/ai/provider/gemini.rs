//! The `gemini` provider: Google's `generateContent`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use url::Url;

use super::http::{Auth, Http};
use super::profile::{self, Profile, Reasoning};
use super::{Capabilities, Error, count};
use crate::ai::request::{
    Media, Message, Native, Output, Part, Request, Response, Role, ToolCall, Usage,
};
use crate::config::{AiProvider, Secret};

pub(super) const BASE: &str = "https://generativelanguage.googleapis.com/";
/// Starts the ID of a call Gemini gave none, which isn't sent back
const LOCAL_ID: &str = "local-";
/// What Gemini documents for a call it didn't make, such as one in another provider's history
const NO_SIGNATURE: &str = "skip_thought_signature_validator";
/// Finish reasons for an answer withheld for safety or policy
const WITHHELD: [&str; 5] = [
    "SAFETY",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "RECITATION",
];
/// Names of models built for something other than conversation: images, speech, embeddings
const SPECIALISED: [&str; 4] = ["-image", "-tts", "live", "embedding"];

#[derive(Clone, Debug)]
pub struct Gemini {
    http: Http,
    model: String,
    profile: Profile,
}

impl Gemini {
    pub fn new(model: &str, key: &Secret, base_url: Option<&Url>) -> Result<Self, String> {
        let base = base_url
            .cloned()
            .unwrap_or_else(|| Url::parse(BASE).expect("a URL"));
        // Written either way in Google's examples
        let model = model.strip_prefix("models/").unwrap_or(model);
        Ok(Self {
            http: Http::new(base, key, Auth::Header("x-goog-api-key"), &[])?,
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
        let method = format!("{}:generateContent", self.model);
        let url = self.http.url(&["v1beta", "models", &method]);
        let body = self.body(request);
        let answer = self.http.post(url, &body, request.timeout).await;
        read(&self.model, answer?)
    }

    fn body(&self, request: &Request) -> Value {
        let mut config = json!({
            "maxOutputTokens": self.profile.output_limit(request.max_output_tokens),
        });
        if self.profile.temperature {
            config["temperature"] = json!(request.temperature);
        }
        match self.profile.reasoning {
            Some(Reasoning::Effort(level)) => {
                config["thinkingConfig"] = json!({ "thinkingLevel": level });
            }
            Some(Reasoning::Budget(tokens)) => {
                config["thinkingConfig"] = json!({ "thinkingBudget": tokens });
            }
            None => {}
        }
        let mut body = json!({ "contents": contents(&self.model, &request.messages) });
        if !request.system.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": request.system }] });
        }
        match &request.output {
            Output::Text => {}
            Output::Tools(tools) => {
                let declarations: Vec<Value> = tools
                    .iter()
                    .map(|tool| {
                        // `parameters` takes no `additionalProperties`
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "parametersJsonSchema": tool.parameters.to_json(),
                        })
                    })
                    .collect();
                body["tools"] = json!([{ "functionDeclarations": declarations }]);
                body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "AUTO" } });
            }
            Output::Json(schema) => {
                config["responseMimeType"] = json!("application/json");
                config["responseJsonSchema"] = schema.to_json();
            }
        }
        body["generationConfig"] = config;
        body
    }
}

/// Every Gemini model for conversation takes images and audio. 2.x models take a temperature
/// and a thinking budget; from 3 on, Google advises against a temperature (ignored, and later
/// a 400) and the thinking level is set instead, low for short replies due within 10 s
/// (FR-AI-07, NFR-PERF-03).
fn profile(model: &str) -> Profile {
    let conversational = !SPECIALISED.iter().any(|marker| model.contains(marker));
    let capabilities = if conversational {
        Capabilities::ALL
    } else {
        Capabilities::default()
    };
    let older = profile::version(model).is_some_and(|version| version < (3, 0));
    let (reasoning, thinks) = if !older {
        (Some(Reasoning::Effort("low")), true)
    } else if model.contains("-pro") {
        // 2.5 Pro can't stop thinking; 128 is its least
        (Some(Reasoning::Budget(128)), true)
    } else if model.contains("-flash-lite") {
        // Doesn't think unless asked
        (None, false)
    } else if model.contains("-flash") {
        (Some(Reasoning::Budget(0)), false)
    } else {
        (None, false)
    };
    Profile {
        capabilities,
        temperature: older,
        reasoning,
        thinks,
    }
}

/// The conversation as Gemini's `user` and `model` turns. Tool results go in a `user` turn,
/// all of one step's together, as Gemini requires.
fn contents(model: &str, messages: &[Message]) -> Vec<Value> {
    let mut turns: Vec<(&str, Vec<Value>)> = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User | Role::Tool => "user",
            Role::Assistant => "model",
        };
        let parts = match own_turn(model, message) {
            Some(parts) => parts.clone(),
            None => rebuilt(message, messages),
        };
        if parts.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, content)) if *last == role => content.extend(parts),
            _ => turns.push((role, parts)),
        }
    }
    turns
        .into_iter()
        .map(|(role, parts)| json!({ "role": role, "parts": parts }))
        .collect()
}

/// The parts Gemini sent for this turn, when it was this model's; they keep its thought
/// signatures.
fn own_turn<'a>(model: &str, message: &'a Message) -> Option<&'a Vec<Value>> {
    message.parts.iter().find_map(|part| match part {
        Part::Native(native)
            if native.provider == AiProvider::Gemini.name() && native.model == model =>
        {
            native.content["parts"].as_array()
        }
        _ => None,
    })
}

/// A message as Gemini parts. The first call in a turn made elsewhere carries the documented
/// stand-in for a thought signature, which Gemini 3 requires.
fn rebuilt(message: &Message, messages: &[Message]) -> Vec<Value> {
    let mut signed = false;
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) if text.is_empty() => None,
            Part::Text(text) => Some(json!({ "text": text })),
            Part::Image(media) | Part::Audio(media) => Some(json!({
                "inlineData": { "mimeType": mime_type(media), "data": STANDARD.encode(&media.bytes) },
            })),
            Part::ToolCall(call) => {
                let mut part = json!({ "functionCall": function(&call.id, &call.name) });
                part["functionCall"]["args"] = if call.arguments.is_object() {
                    call.arguments.clone()
                } else {
                    json!({})
                };
                if !signed {
                    part["thoughtSignature"] = json!(NO_SIGNATURE);
                    signed = true;
                }
                Some(part)
            }
            Part::ToolResult { call_id, content } => {
                let mut response = function(call_id, name_of(call_id, messages));
                response["response"] = match content {
                    Value::Object(_) => content.clone(),
                    content => json!({ "output": content }),
                };
                Some(json!({ "functionResponse": response }))
            }
            // Another provider's turn; this one is rebuilt from the other parts
            Part::Native(_) => None,
        })
        .collect()
}

/// `{id, name}`, without an ID Gemini didn't give
fn function(id: &str, name: &str) -> Value {
    let mut function = Map::new();
    if !id.starts_with(LOCAL_ID) {
        function.insert("id".to_owned(), json!(id));
    }
    function.insert("name".to_owned(), json!(name));
    Value::Object(function)
}

/// The name of the tool the call `id` named, which a function response must repeat
fn name_of<'a>(id: &str, messages: &'a [Message]) -> &'a str {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .find_map(|part| match part {
            Part::ToolCall(call) if call.id == id => Some(call.name.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

/// LINE reports voice messages in ways Gemini doesn't list; it takes `audio/m4a`.
fn mime_type(media: &Media) -> &str {
    match media.media_type.as_str() {
        "audio/x-m4a" | "audio/mp4" | "audio/aac-mp4" => "audio/m4a",
        other => other,
    }
}

fn read(model: &str, answer: Value) -> Result<Response, Error> {
    if let Some(reason) = answer
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
    {
        return Err(Error::Declined(format!(
            "the prompt was blocked ({reason})"
        )));
    }
    let Some(candidate) = answer.pointer("/candidates/0") else {
        return Err(Error::Malformed("no candidate".to_owned()));
    };
    match candidate["finishReason"].as_str() {
        None | Some("STOP") => {}
        Some("MAX_TOKENS") => return Err(Error::Malformed(super::CUT_OFF.to_owned())),
        Some(reason) if WITHHELD.contains(&reason) || reason.starts_with("IMAGE_") => {
            return Err(Error::Declined(format!("withheld ({reason})")));
        }
        Some(reason) => return Err(Error::Malformed(format!("the model stopped with {reason}"))),
    }
    let content = &candidate["content"];
    let parts = content["parts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for part in parts {
        if let Some(call) = part.get("functionCall") {
            let id = match call["id"].as_str() {
                Some(id) => id.to_owned(),
                None => format!("{LOCAL_ID}{}", tool_calls.len() + 1),
            };
            tool_calls.push(ToolCall {
                id,
                name: call["name"].as_str().unwrap_or_default().to_owned(),
                arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        } else if part["thought"] != true
            && let Some(part) = part["text"].as_str()
        {
            text.push_str(part);
        }
    }
    let usage = &answer["usageMetadata"];
    Ok(Response {
        text,
        tool_calls,
        usage: Usage {
            input_tokens: count(&usage["promptTokenCount"])
                .saturating_add(count(&usage["toolUsePromptTokenCount"])),
            // Thinking is billed as output
            output_tokens: count(&usage["candidatesTokenCount"])
                .saturating_add(count(&usage["thoughtsTokenCount"])),
        },
        native: Some(Native {
            provider: AiProvider::Gemini.name(),
            model: model.to_owned(),
            content: content.clone(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ai::request::Tool;
    use crate::ai::schema::Schema;

    fn gemini(model: &str) -> Gemini {
        Gemini::new(model, &Secret::new("key"), None).unwrap()
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

    fn config(model: &str) -> Value {
        gemini(model).body(&request(vec![Message::user("x")], Output::Text))["generationConfig"]
            .clone()
    }

    #[test]
    fn thinking_is_set_the_way_each_generation_takes_it() {
        assert_eq!(
            config("gemini-2.5-flash"),
            json!({ "maxOutputTokens": 1024, "temperature": 0.3, "thinkingConfig": { "thinkingBudget": 0 } })
        );
        assert_eq!(
            config("gemini-2.5-pro"),
            json!({ "maxOutputTokens": 1024 + profile::THINKING_ALLOWANCE, "temperature": 0.3, "thinkingConfig": { "thinkingBudget": 128 } })
        );
        assert_eq!(
            config("gemini-2.5-flash-lite"),
            json!({ "maxOutputTokens": 1024, "temperature": 0.3 })
        );
        for model in ["gemini-3.8-flash", "gemini-4-argon", "gemini-next"] {
            assert_eq!(
                config(model),
                json!({ "maxOutputTokens": 1024 + profile::THINKING_ALLOWANCE, "thinkingConfig": { "thinkingLevel": "low" } }),
                "{model}"
            );
        }
    }

    #[test]
    fn conversational_models_take_everything() {
        assert_eq!(gemini("gemini-3.8-flash").capabilities(), Capabilities::ALL);
        assert_eq!(gemini("models/gemini-2.5-pro").model(), "gemini-2.5-pro");
        for model in [
            "gemini-3.1-flash-image",
            "gemini-2.5-flash-preview-tts",
            "gemini-embedding-001",
        ] {
            assert_eq!(
                gemini(model).capabilities(),
                Capabilities::default(),
                "{model}"
            );
        }
    }

    #[test]
    fn a_turn_made_elsewhere_is_rebuilt_with_the_stand_in_signature() {
        let call = |id: &str| ToolCall {
            id: id.to_owned(),
            name: "get_cart".to_owned(),
            arguments: json!({}),
        };
        let messages = vec![
            Message::user("ตะกร้ามีอะไร"),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::ToolCall(call("toolu_1")),
                    Part::ToolCall(call("local-2")),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    call_id: "toolu_1".to_owned(),
                    content: json!({ "items": 2 }),
                }],
            },
            Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    call_id: "local-2".to_owned(),
                    content: json!("ว่าง"),
                }],
            },
        ];
        let body = gemini("gemini-3.8-flash").body(&request(messages, Output::Text));
        assert_eq!(
            body["contents"],
            json!([
                { "role": "user", "parts": [{ "text": "ตะกร้ามีอะไร" }] },
                { "role": "model", "parts": [
                    { "functionCall": { "id": "toolu_1", "name": "get_cart", "args": {} }, "thoughtSignature": NO_SIGNATURE },
                    { "functionCall": { "name": "get_cart", "args": {} } },
                ] },
                // Both responses in one turn
                { "role": "user", "parts": [
                    { "functionResponse": { "id": "toolu_1", "name": "get_cart", "response": { "items": 2 } } },
                    { "functionResponse": { "name": "get_cart", "response": { "output": "ว่าง" } } },
                ] },
            ])
        );
        assert_eq!(
            body["systemInstruction"],
            json!({ "parts": [{ "text": "กติกา" }] })
        );
    }

    #[test]
    fn its_own_turn_goes_back_unchanged() {
        let content = json!({ "role": "model", "parts": [
            { "functionCall": { "id": "c1", "name": "get_cart", "args": {} }, "thoughtSignature": "real" },
        ] });
        let turn = Message {
            role: Role::Assistant,
            parts: vec![
                Part::ToolCall(ToolCall {
                    id: "c1".to_owned(),
                    name: "get_cart".to_owned(),
                    arguments: json!({}),
                }),
                Part::Native(Native {
                    provider: "gemini",
                    model: "gemini-3.8-flash".to_owned(),
                    content: content.clone(),
                }),
            ],
        };
        let body = gemini("gemini-3.8-flash").body(&request(vec![turn], Output::Text));
        assert_eq!(body["contents"][0]["parts"], content["parts"]);
    }

    #[test]
    fn media_is_sent_inline_and_voice_as_m4a() {
        let message = Message {
            role: Role::User,
            parts: vec![
                Part::Image(Media {
                    media_type: "image/jpeg".to_owned(),
                    bytes: vec![1, 2, 3],
                }),
                Part::Audio(Media {
                    media_type: "audio/x-m4a".to_owned(),
                    bytes: vec![4],
                }),
            ],
        };
        let body = gemini("gemini-3.8-flash").body(&request(vec![message], Output::Text));
        assert_eq!(
            body["contents"][0]["parts"],
            json!([
                { "inlineData": { "mimeType": "image/jpeg", "data": "AQID" } },
                { "inlineData": { "mimeType": "audio/m4a", "data": "BA==" } },
            ])
        );
    }

    #[test]
    fn tools_and_json_use_json_schema_as_emitted() {
        let schema = Schema::object().property("name", Schema::string().max_length(10));
        let tool = Tool {
            name: "find".to_owned(),
            description: "Find".to_owned(),
            parameters: schema.clone(),
        };
        let tools = gemini("gemini-3.8-flash").body(&request(
            vec![Message::user("x")],
            Output::Tools(vec![tool]),
        ));
        assert_eq!(
            tools["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            schema.to_json()
        );
        assert_eq!(
            tools["toolConfig"],
            json!({ "functionCallingConfig": { "mode": "AUTO" } })
        );

        let json_ = gemini("gemini-3.8-flash").body(&request(
            vec![Message::user("x")],
            Output::Json(schema.clone()),
        ));
        assert_eq!(
            json_["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            json_["generationConfig"]["responseJsonSchema"],
            schema.to_json()
        );
    }

    #[test]
    fn answers_skip_thoughts_and_keep_the_native_turn() {
        let answer = json!({
            "candidates": [{
                "content": { "role": "model", "parts": [
                    { "text": "คิดก่อน", "thought": true },
                    { "text": "ดูให้นะคะ" },
                    { "functionCall": { "name": "get_cart", "args": { "a": 1 } }, "thoughtSignature": "sig" },
                ] },
                "finishReason": "STOP",
            }],
            "usageMetadata": { "promptTokenCount": 100, "candidatesTokenCount": 10, "thoughtsTokenCount": 40 },
        });
        let response = read("gemini-2.5-pro", answer.clone()).unwrap();
        assert_eq!(response.text, "ดูให้นะคะ");
        assert_eq!(
            response.tool_calls,
            [ToolCall {
                id: "local-1".to_owned(),
                name: "get_cart".to_owned(),
                arguments: json!({ "a": 1 })
            }]
        );
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 50
            }
        );
        assert_eq!(
            response.native.unwrap().content,
            answer["candidates"][0]["content"]
        );
    }

    #[test]
    fn blocked_withheld_and_cut_off_answers_are_errors() {
        let blocked = json!({ "promptFeedback": { "blockReason": "PROHIBITED_CONTENT" } });
        assert_eq!(
            read("m", blocked).unwrap_err().to_string(),
            "the model declined to answer: the prompt was blocked (PROHIBITED_CONTENT)"
        );
        let finished = |reason: &str| {
            read(
                "m",
                json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": reason }] }),
            )
        };
        assert!(matches!(finished("SAFETY"), Err(Error::Declined(_))));
        assert!(matches!(finished("IMAGE_SAFETY"), Err(Error::Declined(_))));
        assert!(matches!(finished("MAX_TOKENS"), Err(Error::Malformed(_))));
        let Err(Error::Malformed(why)) = finished("MALFORMED_FUNCTION_CALL") else {
            panic!("accepted");
        };
        assert_eq!(why, "the model stopped with MALFORMED_FUNCTION_CALL");
        assert!(matches!(
            read("m", json!({ "candidates": [] })),
            Err(Error::Malformed(_))
        ));
    }
}
