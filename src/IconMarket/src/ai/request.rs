//! The provider-neutral request and response (FR-AIP-02). Adapters translate them to and from
//! their provider's API, so nothing outside the gateway sees a provider's format.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

use super::schema::Schema;

/// One request to the model.
#[derive(Clone, Debug)]
pub struct Request {
    /// The system instructions
    pub system: String,
    /// The conversation, oldest first. Consecutive messages may share a role; an adapter
    /// merges them if its provider needs that.
    pub messages: Vec<Message>,
    pub output: Output,
    pub max_output_tokens: u32,
    /// 0 to 2
    pub temperature: f64,
    /// The gateway gives up after this long, whatever the adapter does
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            parts: vec![Part::Text(text.into())],
        }
    }

    /// The text parts, one per line
    pub fn text(&self) -> String {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n")
    }
}

/// Who a message is from. Matches the roles of the conversation memory (FR-AI-11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// The results of the model's tool calls
    Tool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Image(Media),
    Audio(Media),
    /// A call the model made, kept in the conversation
    ToolCall(ToolCall),
    /// What the system's run of a tool call returned
    ToolResult {
        call_id: String,
        content: Value,
    },
}

/// An image or audio clip sent to the model.
#[derive(Clone, PartialEq)]
pub struct Media {
    /// e.g. `image/jpeg`
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// The type and size only: the bytes are user content (NFR-PRIV-05).
impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}, {} bytes>", self.media_type, self.bytes.len())
    }
}

/// What the model is asked to answer with.
#[derive(Clone, Debug)]
pub enum Output {
    Text,
    /// Text, or calls to some of these tools
    Tools(Vec<Tool>),
    /// One JSON value matching the schema: structured output, such as a draft
    Json(Schema),
}

/// A function the model may call; the system runs it (FR-AI-02).
#[derive(Clone, Debug)]
pub struct Tool {
    pub name: String,
    /// What it does and when to use it, for the model
    pub description: String,
    /// An object schema
    pub parameters: Schema,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The provider's ID, which the result must quote
    pub id: String,
    pub name: String,
    /// As the model wrote them. An adapter keeps arguments that aren't JSON as a string, so
    /// that validation reports them.
    pub arguments: Value,
}

/// What an adapter returns, before validation.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// Tokens the provider counted, for cost (NFR-COST-02); 0 when it didn't say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
