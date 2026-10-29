//! The `mock` provider (FR-AIP-05): deterministic answers with no network and no key, for
//! development and tests. It accepts every kind of input.
//!
//! It answers from the request alone:
//! - free text: the user's last message, echoed
//! - JSON: the smallest value the schema allows
//! - tools: text, unless the user's last message is `/tool_name {"arg": 1}` naming an offered
//!   tool, which becomes a call to it, so tool flows can be tried by hand; once the tool's
//!   result is back, text again, so the assistant's rounds end
//!
//! Tests script it instead ([`Mock::scripted`]): each request takes the next [`Step`], and
//! every request is recorded.

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{Capabilities, Error};
use crate::ai::request::{Message, Output, Request, Response, Role, Tool, ToolCall, Usage};

/// Roughly how many characters make a token, for the usage the mock reports
const CHARS_PER_TOKEN: usize = 4;

#[derive(Clone, Debug)]
pub struct Mock {
    model: String,
    capabilities: Capabilities,
    #[cfg(test)]
    script: Option<Arc<Script>>,
}

/// What a scripted mock does with one request.
#[cfg(test)]
#[derive(Debug)]
pub enum Step {
    Reply(Response),
    Fail(Error),
    /// Never answers, so the gateway's timeout fires
    Hang,
}

#[cfg(test)]
#[derive(Debug)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Request>>,
}

impl Mock {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            capabilities: Capabilities::ALL,
            #[cfg(test)]
            script: None,
        }
    }

    /// A mock that answers with `steps` in turn and records every request. Clones share both.
    #[cfg(test)]
    pub fn scripted(steps: impl IntoIterator<Item = Step>) -> Self {
        let script = Script {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::default(),
        };
        Self {
            script: Some(Arc::new(script)),
            ..Self::new("mock-model")
        }
    }

    /// The same mock, accepting only `capabilities`
    #[cfg(test)]
    pub fn with_capabilities(self, capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            ..self
        }
    }

    /// Every request a scripted mock has received, in order
    #[cfg(test)]
    pub fn requests(&self) -> Vec<Request> {
        self.script
            .as_ref()
            .map_or_else(Vec::new, |script| script.requests.lock().unwrap().clone())
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    pub async fn complete(&self, request: &Request) -> Result<Response, Error> {
        #[cfg(test)]
        if let Some(script) = &self.script {
            return script.next(request).await;
        }
        Ok(answer(request))
    }
}

#[cfg(test)]
impl Script {
    async fn next(&self, request: &Request) -> Result<Response, Error> {
        self.requests.lock().unwrap().push(request.clone());
        let step = self.steps.lock().unwrap().pop_front();
        match step.expect("the mock's script has no step left") {
            Step::Reply(response) => Ok(response),
            Step::Fail(error) => Err(error),
            Step::Hang => std::future::pending().await,
        }
    }
}

fn answer(request: &Request) -> Response {
    let last = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(Message::text)
        .unwrap_or_default();
    let (text, tool_calls) = match &request.output {
        Output::Text => (echo(&last), Vec::new()),
        Output::Json(schema) => (schema.example().to_string(), Vec::new()),
        Output::Tools(tools) => {
            let answered = request
                .messages
                .last()
                .is_some_and(|message| message.role == Role::Tool);
            match command(&last, tools) {
                Some(call) if !answered => (String::new(), vec![call]),
                _ => (echo(&last), Vec::new()),
            }
        }
    };

    let prompt = request.system.chars().count()
        + request
            .messages
            .iter()
            .map(|message| message.text().chars().count())
            .sum::<usize>();
    let reply = text.chars().count()
        + tool_calls
            .iter()
            .map(|call| call.arguments.to_string().chars().count())
            .sum::<usize>();
    Response {
        text,
        tool_calls,
        usage: Usage {
            input_tokens: tokens(prompt),
            output_tokens: tokens(reply),
        },
        native: None,
    }
}

fn echo(last: &str) -> String {
    format!("(mock) {last}")
}

/// `/name {"arg": 1}` as a call to the offered tool `name`; `None` for anything else.
/// Arguments that aren't JSON are passed on as a string, as an adapter does, so validation
/// can be tried by hand too.
fn command(text: &str, tools: &[Tool]) -> Option<ToolCall> {
    let command = text.trim().strip_prefix('/')?;
    let (name, arguments) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    let tool = tools.iter().find(|tool| tool.name == name)?;
    let arguments = match arguments.trim() {
        "" => json!({}),
        arguments => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_owned()))
        }
    };
    Some(ToolCall {
        id: "mock-call-1".to_owned(),
        name: tool.name.clone(),
        arguments,
    })
}

fn tokens(chars: usize) -> u32 {
    u32::try_from(chars.div_ceil(CHARS_PER_TOKEN)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ai::request::Part;
    use crate::ai::schema::Schema;

    fn request(text: &str, output: Output) -> Request {
        Request {
            system: "You help villagers buy and sell.".to_owned(),
            messages: vec![
                Message::user("ก่อนหน้า"),
                Message::assistant("ได้เลย"),
                Message::user(text),
            ],
            output,
            max_output_tokens: 1024,
            temperature: 0.3,
            timeout: Duration::from_secs(20),
        }
    }

    fn search() -> Output {
        Output::Tools(vec![Tool {
            name: "search_products".to_owned(),
            description: "Search".to_owned(),
            parameters: Schema::object().property("keywords", Schema::string()),
        }])
    }

    async fn ask(text: &str, output: Output) -> Response {
        Mock::new("mock")
            .complete(&request(text, output))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_same_request_gets_the_same_answer() {
        let first = ask("สวัสดี", Output::Text).await;
        assert_eq!(first, ask("สวัสดี", Output::Text).await);
        assert_eq!(first.text, "(mock) สวัสดี");
        assert!(first.tool_calls.is_empty());
        assert!(first.usage.input_tokens > 0 && first.usage.output_tokens > 0);
    }

    #[tokio::test]
    async fn json_answers_match_their_schema() {
        let schema = Schema::object()
            .property("verdict", Schema::string().one_of(["allow", "review"]))
            .property("reason", Schema::string());
        let response = ask("ตรวจสินค้า", Output::Json(schema.clone())).await;
        let value: Value = serde_json::from_str(&response.text).unwrap();
        assert_eq!(schema.validate(&value), Vec::new());
    }

    #[tokio::test]
    async fn a_slash_command_calls_an_offered_tool() {
        let response = ask(r#"/search_products {"keywords": "ไข่ไก่"}"#, search()).await;
        assert_eq!(response.text, "");
        assert_eq!(
            response.tool_calls,
            [ToolCall {
                id: "mock-call-1".to_owned(),
                name: "search_products".to_owned(),
                arguments: json!({ "keywords": "ไข่ไก่" }),
            }]
        );

        let bare = ask("/search_products", search()).await;
        assert_eq!(bare.tool_calls[0].arguments, json!({}));
        let broken = ask("/search_products {keywords", search()).await;
        assert_eq!(broken.tool_calls[0].arguments, json!("{keywords"));

        // Anything else is answered with text
        for text in ["/get_cart", "search_products", "หาไข่ไก่"] {
            let response = ask(text, search()).await;
            assert!(response.tool_calls.is_empty(), "{text}");
            assert_eq!(response.text, format!("(mock) {text}"));
        }
    }

    #[tokio::test]
    async fn the_result_of_a_call_is_answered_with_text() {
        let command = r#"/search_products {"keywords": "ไข่ไก่"}"#;
        let mut request = request(command, search());
        let call = Mock::new("mock").complete(&request).await.unwrap();
        request.messages.push(Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall(call.tool_calls[0].clone())],
        });
        request.messages.push(Message {
            role: Role::Tool,
            parts: vec![Part::ToolResult {
                call_id: "mock-call-1".to_owned(),
                content: json!({ "found": 2 }),
            }],
        });

        let response = Mock::new("mock").complete(&request).await.unwrap();

        assert!(response.tool_calls.is_empty());
        assert_eq!(response.text, format!("(mock) {command}"));
    }
}
