//! The gateway against the scripted mock provider and a migrated database: output validation
//! (FR-AI-05, NFR-MNT-07), the daily limits (FR-AI-09), timeouts and retries (FR-AI-10) and
//! call logging (FR-AI-16). Each test starts a throwaway PostgreSQL, so they need Docker.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use chrono_tz::Asia::Bangkok;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, EntityTrait, QueryOrder};
use serde_json::{Value, json};
use sqlx::PgPool;
use url::Url;

use super::provider::contract::{self, Case, Fixture};
use super::provider::testing::Fake;
use super::provider::{Adapter, Error, Mock, Step};
use super::request::{Response, Usage};
use super::*;
use crate::config::{
    AiConfig, AiEndpoint, AiFeatures, AiLimits, AiProvider, AppConfig, PoliteParticle,
};
use crate::notifications::templates::Templates;
use crate::persistence::entity::{ai_call_log, user};
use crate::persistence::{orm, testing};

/// Long enough for the mock to answer, short enough for a test to wait out
const TIMEOUT: Duration = Duration::from_millis(200);

const SOMCHAI: &str = "U0123456789abcdef0123456789abcdef";
const MALEE: &str = "U89abcdef0123456789abcdef01234567";

fn config(per_user_daily_calls: u32, global_daily_calls: u32) -> AiConfig {
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
            per_user_daily_calls,
            global_daily_calls,
        },
        log_content: false,
        fallback: None,
    }
}

fn gateway(db: &PgPool, mock: &Mock, config: &AiConfig) -> Gateway {
    let adapter = Adapter::Mock(mock.clone());
    Gateway::for_test(config, Some(adapter), db.clone(), TIMEOUT)
}

fn templates() -> Templates {
    Templates::new(&AppConfig {
        name: "IconMarket".to_owned(),
        base_url: Url::parse("https://market.example.com").unwrap(),
        listen_port: 8080,
        time_zone: Bangkok,
        polite_particle: PoliteParticle::Kha,
    })
}

/// A search tool, roughly as the assistant (#37) will offer it
fn search() -> Tool {
    Tool {
        name: "search_products".to_owned(),
        description: "Find products for sale".to_owned(),
        parameters: Schema::object()
            .property("keywords", Schema::string().max_length(50))
            .optional("maxPrice", Schema::number().minimum(0)),
    }
}

/// A buyer's question for the assistant
fn ask(user_id: Option<i64>) -> Call {
    Call {
        feature: Feature::Assistant,
        user_id,
        system: "กติกาของผู้ช่วย".to_owned(),
        messages: vec![Message::user("อยากได้ไข่ไก่ร้านป้าแดง ไม่เกิน 50 บาท")],
        output: Output::Tools(vec![search()]),
    }
}

const USAGE: Usage = Usage {
    input_tokens: 120,
    output_tokens: 8,
};

fn reply(text: &str) -> Step {
    Step::Reply(Response {
        text: text.to_owned(),
        tool_calls: Vec::new(),
        usage: USAGE,
        native: None,
    })
}

fn tool_call(name: &str, arguments: Value) -> Step {
    Step::Reply(Response {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "call-1".to_owned(),
            name: name.to_owned(),
            arguments,
        }],
        usage: USAGE,
        native: None,
    })
}

async fn new_user(db: &PgPool, line_user_id: &str) -> i64 {
    let now = Utc::now();
    let user = user::ActiveModel {
        line_user_id: Set(Some(line_user_id.to_owned())),
        is_admin: Set(false),
        is_following: Set(true),
        status: Set(user::Status::Active),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    user.insert(&orm(db.clone())).await.unwrap().id
}

/// Every call logged, oldest first
async fn logged(db: &PgPool) -> Vec<ai_call_log::Model> {
    ai_call_log::Entity::find()
        .order_by_asc(ai_call_log::Column::Id)
        .all(&orm(db.clone()))
        .await
        .unwrap()
}

async fn outcomes(db: &PgPool) -> Vec<String> {
    logged(db)
        .await
        .into_iter()
        .map(|call| call.outcome)
        .collect()
}

/// AC-05: with `ai.provider` set to claude, switching to gemini with its model and key leaves
/// every AI feature working: the same calls get valid answers from the same database, and no
/// schema or data changes. Each provider answers with its recorded contract fixtures.
#[tokio::test]
async fn switching_provider_is_a_config_change() {
    let (_postgres, db) = testing::migrated().await;
    let calls = [
        (Feature::Assistant, Case::Tools),
        (Feature::Listing, Case::Json),
        (Feature::SlipReading, Case::Image),
    ];
    for provider in [AiProvider::Claude, AiProvider::Gemini] {
        let fake = Fake::start().await;
        let mut model = String::new();
        for (_, case) in calls {
            let fixture = Fixture::expect(provider, case);
            fake.answer(fixture.status, fixture.response);
            model = fixture.model;
        }
        let mut config = config(50, 2000);
        config.primary = Some(contract::endpoint(provider, &model, "ai-key-value"));
        config.base_url = Some(fake.url.clone());
        let gateway = Gateway::new(&config, Bangkok, db.clone());

        for (feature, case) in calls {
            let request = case.request(None);
            let call = Call {
                feature,
                user_id: None,
                system: request.system,
                messages: request.messages,
                output: request.output,
            };
            let answer = gateway.complete(call).await;
            assert!(
                answer.is_ok(),
                "{} {feature:?}: {answer:?}",
                provider.name()
            );
        }
        // Claude takes no audio, so only voice messages wait for a model that does
        for feature in Feature::ALL {
            let expected = feature != Feature::Voice || provider == AiProvider::Gemini;
            assert_eq!(gateway.is_enabled(feature), expected, "{feature:?}");
        }
    }
    let providers: Vec<String> = logged(&db)
        .await
        .into_iter()
        .map(|call| call.provider)
        .collect();
    assert_eq!(
        providers,
        ["claude", "claude", "claude", "gemini", "gemini", "gemini"]
    );
}

/// AC (NFR-MNT-07): a call to a tool that doesn't exist is retried once, with a hint naming
/// the problem, and the corrected answer is used.
#[tokio::test]
async fn an_invalid_tool_call_is_retried_once_with_a_hint() {
    let (_postgres, db) = testing::migrated().await;
    let mock = Mock::scripted([
        tool_call("serch_products", json!({ "keywords": "ไข่ไก่" })),
        tool_call(
            "search_products",
            json!({ "keywords": "ไข่ไก่", "maxPrice": 50 }),
        ),
    ]);

    let answer = gateway(&db, &mock, &config(50, 2000))
        .complete(ask(None))
        .await;

    let Ok(Answer::Tools { calls, .. }) = answer else {
        panic!("not answered: {answer:?}");
    };
    assert_eq!(
        calls[0].arguments,
        json!({ "keywords": "ไข่ไก่", "maxPrice": 50 })
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let (first, second) = (&requests[0], &requests[1]);
    assert_eq!(second.messages[..first.messages.len()], first.messages[..]);
    let hint = second.messages.last().unwrap();
    assert_eq!(hint.role, Role::User);
    assert!(
        hint.text()
            .contains("there is no tool `serch_products`; the tools are: search_products"),
        "{hint:?}"
    );
    assert_eq!(outcomes(&db).await, ["invalid", "ok"]);
}

/// FR-AI-05: an answer that is still invalid after the hint gets the apology and the menus.
#[tokio::test]
async fn an_answer_invalid_twice_gets_the_apology_and_the_menus() {
    let (_postgres, db) = testing::migrated().await;
    let mock = Mock::scripted([
        tool_call(
            "search_products",
            json!({ "keywords": "ไข่", "maxPrice": "ถูกๆ" }),
        ),
        reply(" "),
    ]);

    let result = gateway(&db, &mock, &config(50, 2000))
        .complete(ask(None))
        .await;

    assert_eq!(result, Err(Unavailable::Failed));
    assert_eq!(mock.requests().len(), 2, "one retry only");
    assert_eq!(outcomes(&db).await, ["invalid", "invalid"]);
    let t = templates();
    assert_eq!(Unavailable::Failed.message(&t), t.ai_unavailable());
}

/// FR-AI-05: a draft must be JSON matching its schema.
#[tokio::test]
async fn drafts_are_validated_against_their_schema() {
    let (_postgres, db) = testing::migrated().await;
    let mock = Mock::scripted([
        reply("ขนมครก กล่องละ 20 บาท"),
        reply(r#"{ "name": "ขนมครก", "price": 20, "category": "ขนม" }"#),
    ]);
    let draft = Schema::object()
        .property("name", Schema::string().max_length(100))
        .property("price", Schema::number().minimum(0))
        .property("category", Schema::string().one_of(["อาหาร", "ขนม"]));
    let call = Call {
        feature: Feature::Listing,
        user_id: None,
        system: "Draft a listing".to_owned(),
        messages: vec![Message::user("ขนมครก กล่องละ 20")],
        output: Output::Json(draft),
    };

    let answer = gateway(&db, &mock, &config(50, 2000)).complete(call).await;

    assert_eq!(
        answer,
        Ok(Answer::Json(
            json!({ "name": "ขนมครก", "price": 20, "category": "ขนม" })
        ))
    );
    let hint = mock.requests()[1].messages.last().unwrap().text();
    assert!(hint.contains("the answer is not valid JSON"), "{hint}");
}

/// AC (FR-AI-09): at a user's daily limit the provider is not called and the user is told the
/// assistant is resting. Others are still answered.
#[tokio::test]
async fn at_the_user_limit_the_provider_is_not_called() {
    let (_postgres, db) = testing::migrated().await;
    let (somchai, malee) = (new_user(&db, SOMCHAI).await, new_user(&db, MALEE).await);
    let mock = Mock::scripted([reply("หนึ่ง"), reply("สอง"), reply("สาม")]);
    let gateway = gateway(&db, &mock, &config(2, 2000));

    for _ in 0..2 {
        assert!(gateway.complete(ask(Some(somchai))).await.is_ok());
    }
    let resting = gateway.complete(ask(Some(somchai))).await;

    assert_eq!(resting, Err(Unavailable::Resting));
    assert_eq!(
        mock.requests().len(),
        2,
        "the provider was called at the limit"
    );
    let t = templates();
    assert_eq!(Unavailable::Resting.message(&t), t.ai_resting());

    assert!(gateway.complete(ask(Some(malee))).await.is_ok());
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(
        outcomes(&db).await,
        ["ok", "ok", "ok"],
        "a call not made is not logged"
    );
}

/// FR-AI-09: at the global limit everyone rests, including calls made for no user.
#[tokio::test]
async fn at_the_global_limit_everyone_rests() {
    let (_postgres, db) = testing::migrated().await;
    let (somchai, malee) = (new_user(&db, SOMCHAI).await, new_user(&db, MALEE).await);
    let mock = Mock::scripted([reply("หนึ่ง"), reply("สอง")]);
    let gateway = gateway(&db, &mock, &config(50, 2));

    assert!(gateway.complete(ask(Some(somchai))).await.is_ok());
    assert!(gateway.complete(ask(Some(malee))).await.is_ok());
    for user_id in [Some(somchai), Some(malee), None] {
        let result = gateway.complete(ask(user_id)).await;
        assert_eq!(result, Err(Unavailable::Resting), "{user_id:?}");
    }
    assert_eq!(mock.requests().len(), 2);
}

/// A request counts once against the limits, however many tries it takes.
#[tokio::test]
async fn retries_count_once_against_the_limits() {
    let (_postgres, db) = testing::migrated().await;
    let somchai = new_user(&db, SOMCHAI).await;
    let mock = Mock::scripted([
        Step::Fail(Error::Server { status: 503 }),
        tool_call("serch_products", json!({})),
        reply("ได้เลยค่ะ"),
    ]);
    let gateway = gateway(&db, &mock, &config(1, 2000));

    assert!(gateway.complete(ask(Some(somchai))).await.is_ok());
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(
        gateway.complete(ask(Some(somchai))).await,
        Err(Unavailable::Resting)
    );
}

/// AC (FR-AI-10): a request that timed out is not retried.
#[tokio::test]
async fn a_timed_out_request_is_not_retried() {
    let (_postgres, db) = testing::migrated().await;
    let mock = Mock::scripted([Step::Hang, reply("ช้าไป")]);

    let result = gateway(&db, &mock, &config(50, 2000))
        .complete(ask(None))
        .await;

    assert_eq!(result, Err(Unavailable::Failed));
    assert_eq!(mock.requests().len(), 1, "a timeout was retried");
    let calls = logged(&db).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].outcome, "timeout");
    assert!(calls[0].latency_ms >= 200, "{}", calls[0].latency_ms);
}

/// FR-AI-10: a rate limit or server error is tried again once.
#[tokio::test]
async fn rate_limits_and_server_errors_are_retried_once() {
    let (_postgres, db) = testing::migrated().await;
    let mock = Mock::scripted([Step::Fail(Error::RateLimited), reply("ได้เลยค่ะ")]);
    let answer = gateway(&db, &mock, &config(50, 2000))
        .complete(ask(None))
        .await;
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(mock.requests().len(), 2);

    let mock = Mock::scripted([
        Step::Fail(Error::Server { status: 503 }),
        Step::Fail(Error::Server { status: 502 }),
        reply("ไม่ได้ถามแล้ว"),
    ]);
    let result = gateway(&db, &mock, &config(50, 2000))
        .complete(ask(None))
        .await;
    assert_eq!(result, Err(Unavailable::Failed));
    assert_eq!(mock.requests().len(), 2);

    assert_eq!(
        outcomes(&db).await,
        ["rate_limited", "ok", "server_error", "server_error"]
    );
}

/// FR-AI-10: any other error would only fail again.
#[tokio::test]
async fn other_errors_are_not_retried() {
    let (_postgres, db) = testing::migrated().await;
    let mock = Mock::scripted([
        Step::Fail(Error::Refused {
            status: 400,
            message: "invalid request".to_owned(),
        }),
        Step::Fail(Error::Network("connection reset".to_owned())),
        Step::Fail(Error::Malformed("not JSON".to_owned())),
    ]);
    let gateway = gateway(&db, &mock, &config(50, 2000));

    for tries in 1..=3 {
        assert_eq!(gateway.complete(ask(None)).await, Err(Unavailable::Failed));
        assert_eq!(mock.requests().len(), tries);
    }
    assert_eq!(outcomes(&db).await, ["refused", "network", "malformed"]);
}

/// FR-AI-16: every call is logged with the provider, model, feature, tokens, latency and
/// outcome.
#[tokio::test]
async fn every_call_is_logged_with_its_usage() {
    let (_postgres, db) = testing::migrated().await;
    let somchai = new_user(&db, SOMCHAI).await;
    let mock = Mock::scripted([reply("มีค่ะ")]);
    let before = Utc::now();

    gateway(&db, &mock, &config(50, 2000))
        .complete(ask(Some(somchai)))
        .await
        .unwrap();

    let calls = logged(&db).await;
    let [call] = calls.as_slice() else {
        panic!("{calls:?}");
    };
    assert_eq!(call.user_id, Some(somchai));
    assert_eq!(
        (
            call.feature.as_str(),
            call.provider.as_str(),
            call.model.as_str()
        ),
        ("assistant", "mock", "mock-model")
    );
    assert_eq!((call.input_tokens, call.output_tokens), (120, 8));
    assert!((0..1000).contains(&call.latency_ms), "{}", call.latency_ms);
    assert_eq!(call.outcome, "ok");
    assert!(call.created_at >= before);
}

/// A log line sink for the tests below
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The log lines written while the gateway answers one question
async fn log_of_one_call(db: &PgPool, log_content: bool) -> Vec<Value> {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mock = Mock::scripted([reply("มีค่ะ ร้านป้าแดงมีไข่ไก่")]);
    let mut config = config(50, 2000);
    config.log_content = log_content;
    gateway(db, &mock, &config)
        .complete(ask(None))
        .await
        .unwrap();

    let bytes = capture.0.lock().unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// FR-AI-16, NFR-PRIV-05: prompts and responses are logged only when `ai.logContent` is on.
#[tokio::test]
async fn content_is_logged_only_when_asked() {
    let (_postgres, db) = testing::migrated().await;

    // Only content lines are checked: the other lines' callsites are shared with tests running
    // on other threads, where a thread-local subscriber may miss them
    let without = log_of_one_call(&db, false).await;
    assert!(
        without
            .iter()
            .all(|line| line["message"] != "AI call content"),
        "{without:#?}"
    );
    for text in ["กติกาของผู้ช่วย", "ร้านป้าแดง"]
    {
        assert!(
            without.iter().all(|line| !line.to_string().contains(text)),
            "{without:#?}"
        );
    }

    let with = log_of_one_call(&db, true).await;
    let content = with
        .iter()
        .find(|line| line["message"] == "AI call content")
        .expect("the content is logged");
    assert_eq!(content["feature"], "assistant");
    assert_eq!(content["system"], "กติกาของผู้ช่วย");
    assert_eq!(content["messages"], "user: อยากได้ไข่ไก่ร้านป้าแดง ไม่เกิน 50 บาท");
    assert_eq!(content["response"], "มีค่ะ ร้านป้าแดงมีไข่ไก่");
    assert_eq!(content["tool_calls"], "");
}
