//! Contract tests: each adapter against its provider's real answers, recorded once with live
//! keys and replayed here, so CI needs no key.
//!
//! A [`Case`] is one neutral request, the same for every provider. Its test sends it through
//! the adapter to a [`Fake`] provider replaying the recording, and checks that
//! - the body sent is the one the live API accepted, so the request can't drift unnoticed;
//! - the recorded answer reads as the case expects.
//!
//! Answers the live APIs can't be made to give on demand (rate limits, outages, refusals) are
//! written from the providers' documentation, marked `"recorded": "docs"`.
//!
//! To record again, write `ai-recording.toml` next to `Cargo.toml` (git-ignored):
//!
//! ```toml
//! [claude]
//! model = "claude-opus-5"
//! apiKey = "…"
//!
//! [gemini]
//! model = "gemini-3.8-flash"
//! apiKey = "…"
//! ```
//!
//! with `[gpt]` and `[grok]` likewise, then run
//! `cargo test record_ai_fixtures -- --ignored --nocapture`. Providers missing from the file
//! are left as they are. Keys travel only in headers, which are never written down.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::testing::Fake;
use super::{Adapter, Capabilities, Error, claude, gemini, responses};
use crate::ai::request::{Media, Message, Output, Part, Request, Response, Role, Tool};
use crate::ai::schema::Schema;
use crate::ai::validate;
use crate::config::{AiEndpoint, AiProvider, Secret};

const KEY: &str = "ai-key-value";
/// A key the providers reject, for [`Case::BadKey`]
const WRONG_KEY: &str = "wrong-key";
/// Long enough for a live API
const TIMEOUT: Duration = Duration::from_secs(120);

const SYSTEM: &str = "คุณเป็นผู้ช่วยของตลาดชุมชนออนไลน์ ตอบสั้น ๆ เป็นภาษาไทยที่สุภาพ \
                      เมื่อผู้ใช้ถามหาสินค้า ให้ใช้เครื่องมือค้นหาสินค้าก่อนตอบเสมอ";

pub const PROVIDERS: [AiProvider; 4] = [
    AiProvider::Claude,
    AiProvider::Gemini,
    AiProvider::Gpt,
    AiProvider::Grok,
];

/// One canonical request and what its answer must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// A short conversation, answered with text
    Text,
    /// A product question with the search tool offered
    Tools,
    /// [`Case::Tools`] continued with the call's result: proves the provider's own turn goes
    /// back as it needs (thinking, thought signatures, reasoning items)
    ToolResult,
    /// A listing draft as structured output
    Json,
    /// A payment slip read into structured output
    Image,
    /// A voice message, for models that hear them
    Audio,
    /// A long essay asked for with a tiny output limit
    Truncated,
    /// A key the provider rejects
    BadKey,
    /// From the docs: 429, the rate limit
    RateLimited,
    /// From the docs: 429 for billing (OpenAI; xAI documents none)
    Billing,
    /// From the docs: 5xx
    Overloaded,
    /// From the docs: the model refuses, or the answer is withheld
    Declined,
}

impl Case {
    /// Recorded from the live APIs, in the order they are recorded and replayed
    pub const LIVE: [Self; 8] = [
        Self::Text,
        Self::Tools,
        Self::ToolResult,
        Self::Json,
        Self::Image,
        Self::Audio,
        Self::Truncated,
        Self::BadKey,
    ];
    pub const DOCS: [Self; 4] = [
        Self::RateLimited,
        Self::Billing,
        Self::Overloaded,
        Self::Declined,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Tools => "tools",
            Self::ToolResult => "tool_result",
            Self::Json => "json",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Truncated => "truncated",
            Self::BadKey => "bad_key",
            Self::RateLimited => "rate_limited",
            Self::Billing => "billing",
            Self::Overloaded => "overloaded",
            Self::Declined => "declined",
        }
    }

    /// Whether a model with `capabilities` at `provider` can be asked this
    fn applies(self, provider: AiProvider, capabilities: Capabilities) -> bool {
        match self {
            Self::Image => capabilities.image_input,
            Self::Audio => capabilities.audio_input,
            Self::Billing => provider == AiProvider::Gpt,
            _ => true,
        }
    }

    /// The request. [`Case::ToolResult`] goes on from `turn`, the model's answer to
    /// [`Case::Tools`].
    pub fn request(self, turn: Option<&Message>) -> Request {
        let user = |parts: Vec<Part>| Message {
            role: Role::User,
            parts,
        };
        let text = |text: &str| Part::Text(text.to_owned());
        let (messages, output) = match self {
            Self::Text
            | Self::BadKey
            | Self::RateLimited
            | Self::Billing
            | Self::Overloaded
            | Self::Declined => (
                vec![
                    Message::user("สวัสดีค่ะ"),
                    Message::assistant("สวัสดีค่ะ มีอะไรให้ช่วยไหมคะ"),
                    Message::user("ซื้อของในตลาดนี้จ่ายเงินได้ทางไหนบ้างคะ"),
                ],
                Output::Text,
            ),
            Self::Tools => (
                vec![Message::user("หาไข่ไก่ราคาไม่เกิน 50 บาทให้หน่อยค่ะ")],
                Output::Tools(vec![search()]),
            ),
            Self::ToolResult => {
                let turn = turn.expect("the answer to the tools case").clone();
                let results = turn
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::ToolCall(call) => Some(Part::ToolResult {
                            call_id: call.id.clone(),
                            content: json!({ "products": [{
                                "name": "ไข่ไก่เบอร์ 2 แผง 30 ฟอง",
                                "price": 45,
                                "shop": "ร้านป้าแดง",
                            }] }),
                        }),
                        _ => None,
                    })
                    .collect();
                let mut messages = Self::Tools.request(None).messages;
                messages.push(turn);
                messages.push(Message {
                    role: Role::Tool,
                    parts: results,
                });
                (messages, Output::Tools(vec![search()]))
            }
            Self::Json => (
                vec![Message::user(
                    "ช่วยร่างประกาศขายให้หน่อย: ขนมครกใบเตย กล่องละ 20 บาท ทำสดทุกเช้า มี 30 กล่อง",
                )],
                Output::Json(listing()),
            ),
            Self::Image => (
                vec![user(vec![
                    text("อ่านสลิปโอนเงินนี้ให้หน่อย"),
                    Part::Image(media("slip.jpg", "image/jpeg")),
                ])],
                Output::Json(slip()),
            ),
            Self::Audio => (
                vec![user(vec![
                    text("ฟังข้อความเสียงนี้ แล้วบอกว่าลูกค้าต้องการอะไร"),
                    Part::Audio(media("voice.m4a", "audio/m4a")),
                ])],
                Output::Json(heard()),
            ),
            Self::Truncated => (
                vec![Message::user(
                    "เขียนเรียงความเรื่องตลาดในหมู่บ้าน ยาวอย่างน้อย 3000 คำ",
                )],
                Output::Text,
            ),
        };
        Request {
            system: SYSTEM.to_owned(),
            messages,
            output,
            max_output_tokens: if self == Self::Truncated { 5 } else { 1024 },
            temperature: 0.3,
            timeout: TIMEOUT,
        }
    }

    /// Whether `result` is what this case must get from the adapter
    fn check(self, request: &Request, result: &Result<Response, Error>) -> Result<(), String> {
        let valid = |response: &Response| {
            validate::check(&request.output, response)
                .map(drop)
                .map_err(|problems| problems.join("; "))
        };
        match (self, result) {
            (Self::Text | Self::Audio, Ok(response)) => valid(response),
            (Self::Tools, Ok(response)) => {
                valid(response)?;
                match response.tool_calls.as_slice() {
                    [call] if call.name == "search_products" && response.native.is_some() => Ok(()),
                    calls => Err(format!(
                        "expected one search with its native turn: {calls:?}"
                    )),
                }
            }
            (Self::ToolResult | Self::Json | Self::Image, Ok(response)) => valid(response),
            (Self::Truncated, Err(Error::Malformed(why))) if why == super::CUT_OFF => Ok(()),
            (
                Self::BadKey,
                Err(Error::Refused {
                    status: 400..=403, ..
                }),
            )
            | (Self::RateLimited, Err(Error::RateLimited))
            | (Self::Billing, Err(Error::Refused { status: 429, .. }))
            | (Self::Overloaded, Err(Error::Server { .. }))
            | (Self::Declined, Err(Error::Declined(_))) => Ok(()),
            (_, Ok(response)) => Err(format!("unexpected answer: {response:?}")),
            (_, Err(e)) => Err(format!("unexpected error: {e}")),
        }
    }
}

/// The assistant's search, roughly as #37 will offer it
fn search() -> Tool {
    Tool {
        name: "search_products".to_owned(),
        description: "ค้นหาสินค้าที่มีขายในตลาด".to_owned(),
        parameters: Schema::object()
            .property(
                "keywords",
                Schema::string().max_length(50).describe("คำค้นสินค้า"),
            )
            .optional(
                "maxPrice",
                Schema::number().minimum(0).describe("ราคาสูงสุด เป็นบาท"),
            ),
    }
}

/// A listing draft, roughly as #30 will ask for one
pub fn listing() -> Schema {
    Schema::object()
        .property("name", Schema::string().max_length(40))
        .property("price", Schema::number().minimum(0).maximum(100_000))
        .property(
            "category",
            Schema::string().one_of(["อาหาร", "ขนม", "ผักผลไม้", "ของใช้"]),
        )
        .optional("stock", Schema::integer().minimum(1))
        .optional("description", Schema::string().max_length(200))
}

/// What slip reading takes from a slip, roughly as #46 will ask for it
pub fn slip() -> Schema {
    Schema::object()
        .property(
            "amount",
            Schema::number().minimum(0).describe("ยอดเงินที่โอน เป็นบาท"),
        )
        .property(
            "succeeded",
            Schema::boolean().describe("สลิปบอกว่าโอนสำเร็จหรือไม่"),
        )
}

/// What voice messages take from a clip, as `ai::voice` asks for it
pub fn heard() -> Schema {
    let heard = Schema::string()
        .max_length(2000)
        .describe("what the user said, word for word");
    Schema::object().optional("heard", heard)
}

fn media(name: &str, media_type: &str) -> Media {
    Media {
        media_type: media_type.to_owned(),
        bytes: std::fs::read(fixtures().join("media").join(name)).unwrap(),
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ai/provider/fixtures")
}

/// One exchange with a provider, as stored.
#[derive(Debug, Deserialize, Serialize)]
pub struct Fixture {
    pub model: String,
    /// The day it was recorded, or `docs`
    pub recorded: String,
    /// The body sent; `null` when written from the docs
    pub request: Value,
    pub status: u16,
    pub response: Value,
}

impl Fixture {
    fn path(provider: AiProvider, case: Case) -> PathBuf {
        fixtures()
            .join(provider.name())
            .join(format!("{}.json", case.name()))
    }

    pub fn load(provider: AiProvider, case: Case) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(provider, case)).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    /// The fixture, or a failure saying how to record it
    pub fn expect(provider: AiProvider, case: Case) -> Self {
        Self::load(provider, case).unwrap_or_else(|| {
            panic!(
                "no recording of {}/{}: run the recorder (see src/ai/provider/contract.rs)",
                provider.name(),
                case.name()
            )
        })
    }

    fn save(&self, provider: AiProvider, case: Case) {
        let path = Self::path(provider, case);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = serde_json::to_string_pretty(self).unwrap() + "\n";
        std::fs::write(path, text).unwrap();
    }
}

pub fn endpoint(provider: AiProvider, model: &str, key: &str) -> AiEndpoint {
    AiEndpoint {
        provider,
        model: model.to_owned(),
        api_key: Some(Secret::new(key)),
    }
}

fn adapter(provider: AiProvider, model: &str, case: Case, base_url: Option<&Url>) -> Adapter {
    let key = if case == Case::BadKey { WRONG_KEY } else { KEY };
    Adapter::new(&endpoint(provider, model, key), base_url).unwrap()
}

/// The header that carries the key
fn auth_header(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::Claude => "x-api-key",
        AiProvider::Gemini => "x-goog-api-key",
        _ => "authorization",
    }
}

/// The path each model's requests go to
fn path(provider: AiProvider, model: &str) -> String {
    match provider {
        AiProvider::Claude => "/v1/messages".to_owned(),
        AiProvider::Gemini => format!("/v1beta/models/{model}:generateContent"),
        _ => "/responses".to_owned(),
    }
}

/// The model's turn after [`Case::Tools`], for [`Case::ToolResult`]
fn tools_turn(request: &Request, result: &Result<Response, Error>) -> Option<Message> {
    let response = result.as_ref().ok()?;
    let answer = validate::check(&request.output, response).ok()?;
    Some(answer.assistant_turn())
}

/// Sends `case`'s request through `provider`'s adapter to a fake replaying `fixture`, checks
/// the request against the recorded one, and returns what the adapter made of the answer.
async fn replay(
    provider: AiProvider,
    case: Case,
    fixture: &Fixture,
    turn: Option<&Message>,
) -> (Request, Result<Response, Error>) {
    let what = format!("{}/{}", provider.name(), case.name());
    let fake = Fake::start().await;
    fake.answer(fixture.status, fixture.response.clone());
    let adapter = adapter(provider, &fixture.model, case, Some(&fake.url));
    let request = case.request(turn);
    let result = adapter.complete(&request).await;

    let exchanges = fake.exchanges();
    let [exchange] = exchanges.as_slice() else {
        panic!("{what}: {} requests", exchanges.len());
    };
    if !fixture.request.is_null() {
        assert_eq!(
            exchange.request, fixture.request,
            "{what}: the request differs from the recorded one; record again if that is intended"
        );
        assert_eq!(exchange.path, path(provider, &fixture.model), "{what}");
    }
    assert!(
        exchange.headers.contains_key(auth_header(provider)),
        "{what}: no key sent"
    );
    if let Err(problem) = case.check(&request, &result) {
        panic!("{what}: {problem}");
    }
    (request, result)
}

/// Replays every live recording of `provider`, in order.
async fn answers_as_recorded(provider: AiProvider) {
    let model = Fixture::expect(provider, Case::Text).model;
    let capabilities = adapter(provider, &model, Case::Text, None).capabilities();
    let mut turn = None;
    for case in Case::LIVE {
        if case.applies(provider, capabilities) {
            let fixture = Fixture::expect(provider, case);
            let (request, result) = replay(provider, case, &fixture, turn.as_ref()).await;
            if case == Case::Tools {
                turn = tools_turn(&request, &result);
            }
        }
    }
}

/// Replays the answers written from `provider`'s docs.
async fn errors_read_as_documented(provider: AiProvider) {
    for case in Case::DOCS {
        if case.applies(provider, Capabilities::ALL) {
            // `replay` checks the answer
            let _ = replay(provider, case, &Fixture::expect(provider, case), None).await;
        }
    }
}

#[tokio::test]
async fn claude_answers_as_recorded() {
    answers_as_recorded(AiProvider::Claude).await;
}

#[tokio::test]
async fn gemini_answers_as_recorded() {
    answers_as_recorded(AiProvider::Gemini).await;
}

#[tokio::test]
async fn gpt_answers_as_recorded() {
    answers_as_recorded(AiProvider::Gpt).await;
}

#[tokio::test]
async fn grok_answers_as_recorded() {
    answers_as_recorded(AiProvider::Grok).await;
}

#[tokio::test]
async fn claude_errors_read_as_documented() {
    errors_read_as_documented(AiProvider::Claude).await;
}

#[tokio::test]
async fn gemini_errors_read_as_documented() {
    errors_read_as_documented(AiProvider::Gemini).await;
}

#[tokio::test]
async fn gpt_errors_read_as_documented() {
    errors_read_as_documented(AiProvider::Gpt).await;
}

#[tokio::test]
async fn grok_errors_read_as_documented() {
    errors_read_as_documented(AiProvider::Grok).await;
}

/// The provider's own API, which the recorder passes requests on to
fn upstream(provider: AiProvider) -> Url {
    let base = match provider {
        AiProvider::Claude => claude::BASE,
        AiProvider::Gemini => gemini::BASE,
        AiProvider::Gpt => responses::Flavor::Gpt.base(),
        AiProvider::Grok => responses::Flavor::Grok.base(),
        AiProvider::Mock => unreachable!("the mock has no API"),
    };
    Url::parse(base).unwrap()
}

/// Records every live case for each provider in `ai-recording.toml`; see the module docs.
#[tokio::test]
#[ignore = "calls the live APIs with the keys in ai-recording.toml"]
async fn record_ai_fixtures() {
    let settings = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ai-recording.toml");
    let settings: toml::Table = std::fs::read_to_string(&settings)
        .expect("ai-recording.toml next to Cargo.toml")
        .parse()
        .unwrap();
    let today = chrono::Utc::now().date_naive().to_string();
    for provider in PROVIDERS {
        let Some(section) = settings.get(provider.name()) else {
            println!("{}: not in ai-recording.toml, skipped", provider.name());
            continue;
        };
        let setting = |name: &str| {
            section
                .get(name)
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("{}.{name} is missing", provider.name()))
                .to_owned()
        };
        let (model, key) = (setting("model"), setting("apiKey"));
        let capabilities = adapter(provider, &model, Case::Text, None).capabilities();
        let fake = Fake::forwarding(upstream(provider)).await;
        let mut turn = None;
        for case in Case::LIVE {
            if !case.applies(provider, capabilities) {
                continue;
            }
            if case == Case::ToolResult && turn.is_none() {
                println!(
                    "{}/{}: skipped, no tool call to go on from",
                    provider.name(),
                    case.name()
                );
                continue;
            }
            let key = if case == Case::BadKey {
                WRONG_KEY
            } else {
                &key
            };
            let adapter = Adapter::new(&endpoint(provider, &model, key), Some(&fake.url)).unwrap();
            let request = case.request(turn.as_ref());
            let result = adapter.complete(&request).await;
            let exchange = fake.exchanges().pop().expect("the request was passed on");
            let fixture = Fixture {
                model: model.clone(),
                recorded: today.clone(),
                request: exchange.request,
                status: exchange.status,
                response: exchange.response,
            };
            fixture.save(provider, case);
            let verdict = match case.check(&request, &result) {
                Ok(()) => "ok".to_owned(),
                Err(problem) => format!("NOT AS EXPECTED: {problem}"),
            };
            println!(
                "{}/{}: {} {verdict}",
                provider.name(),
                case.name(),
                fixture.status
            );
            if case == Case::Tools {
                turn = tools_turn(&request, &result);
            }
        }
    }
}
