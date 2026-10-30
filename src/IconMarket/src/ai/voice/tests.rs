//! Voice messages (FR-AI-15): a clip downloaded and written down by the AI, as the user, with
//! nothing asked of the AI for a clip not received or a suspended user, and the user asked to
//! type or use the menu whenever nothing is heard. Each event is handled in a transaction
//! committed once it is handled. The tests each start a throwaway PostgreSQL, so they need
//! Docker.

use chrono::Utc;
use chrono_tz::Asia::Bangkok;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, TransactionTrait};
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::ai::limits as daily;
use crate::ai::request::Request;
use crate::ai::testing::{self as scripted, Model, Turn as Scripted};
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::limits;
use crate::line::testing::{self as fake, Fake};
use crate::notifications::templates::Templates;
use crate::persistence::entity::{ai_call_log, counter, user};
use crate::persistence::{orm, testing};
use crate::users::access::{Actor, Denied};

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const USER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// The message of the clip the user sends, and the path LINE serves it at
const CLIP_ID: &str = "v1";
const CONTENT: &str = "/v2/bot/message/v1/content";
/// A clip of speech, as LINE serves one
const CLIP: &[u8] = include_bytes!("../provider/fixtures/media/voice.m4a");

struct Setup {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    templates: Templates,
    user: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool.clone());
    let user = testing::new_user(USER).insert(&db).await.unwrap().id;
    let config = config::sample();
    Setup {
        _container: container,
        pool,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        user,
    }
}

impl Setup {
    /// The user sends the clip, which LINE serves as `served`, while the AI answers with `says`:
    /// what became of it, and the model, which knows what it was asked. Handled as the worker
    /// does.
    async fn sends(
        &self,
        served: fake::Answer,
        says: impl IntoIterator<Item = Scripted>,
    ) -> (Heard, Model) {
        self.line.script(CONTENT, [served]);
        let (ai, model) = scripted::scripted(&self.pool, says, true, &[]);
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, self.user).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, testing::at())
            .await
            .unwrap();
        let line = self.line.client();
        let heard = hear(&turn, &self.config, &ai, &line, CLIP_ID)
            .await
            .unwrap();
        let replied = match &heard {
            Heard::Reply(messages) => messages.clone(),
            Heard::Said(_) => Vec::new(),
        };
        let Finished { messages, .. } = turn.finish(replied);
        txn.commit().await.unwrap();
        // What is heard goes on to be answered, so has no reply here
        if matches!(heard, Heard::Reply(_)) {
            limits::check(&messages).unwrap();
        }
        (heard, model)
    }

    /// The reply to a clip that isn't heard, and the main menu.
    async fn not_heard(&self) -> Vec<Message> {
        let actor = Actor::load(&self.db, self.user)
            .await
            .unwrap()
            .expect("a user");
        vec![self.templates.voice_not_heard(), menu::main_menu(&actor)]
    }

    /// Whether LINE was asked for the clip
    fn downloaded(&self) -> bool {
        self.line
            .requests()
            .iter()
            .any(|request| request.path == CONTENT)
    }
}

fn clip() -> fake::Answer {
    fake::Answer::bytes("audio/x-m4a", CLIP)
}

fn said(heard: &Heard) -> &str {
    match heard {
        Heard::Said(text) => text,
        Heard::Reply(messages) => panic!("not heard: {messages:?}"),
    }
}

fn reply(heard: Heard) -> Vec<Message> {
    match heard {
        Heard::Reply(messages) => messages,
        Heard::Said(text) => panic!("heard: {text}"),
    }
}

/// The clips the AI is given in `request`.
fn clips(request: &Request) -> Vec<&ai::Media> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::Audio(clip) => Some(clip),
            _ => None,
        })
        .collect()
}

/// FR-AI-15, FR-LINE-09, FR-AI-09, FR-AI-16: the clip is downloaded and sent as it came, in one
/// JSON call with no tools, as the user, which counts once and is logged as voice.
#[tokio::test]
async fn a_clip_is_written_down_by_the_ai() {
    let s = setup().await;
    let says = [Scripted::Json(json!({ "heard": " ขอไข่ไก่ 2 แผง " }))];
    let (heard, model) = s.sends(clip(), says).await;
    assert_eq!(said(&heard), "ขอไข่ไก่ 2 แผง");

    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.messages.len(), 1);
    assert!(matches!(request.output, Output::Json(_)));
    assert!(request.system.contains(DATA_RULE));
    let clips = clips(request);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].media_type, "audio/x-m4a");
    assert_eq!(clips[0].bytes, CLIP);

    let logged = ai_call_log::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].feature, "voice");
    assert_eq!(logged[0].user_id, Some(s.user));
    let counted = ai::calls_today(&s.db, Utc::now(), Bangkok).await;
    assert_eq!(counted.unwrap(), 1);
}

/// LINE may not say what the clip is, or say something that can't be sent as audio.
#[tokio::test]
async fn a_clip_of_no_stated_type_is_taken_for_what_line_records() {
    let s = setup().await;
    for media_type in ["application/octet-stream", "text/plain"] {
        let served = fake::Answer::bytes(media_type, CLIP);
        let says = [Scripted::Json(json!({ "heard": "ไข่" }))];
        let (_, model) = s.sends(served, says).await;
        assert_eq!(clips(&model.requests()[0])[0].media_type, CLIP_TYPE);
    }
}

/// FR-AI-15: no clear speech, however the AI says so, is not a message: the user is asked to
/// type or use the menu.
#[tokio::test]
async fn a_clip_with_no_clear_speech_asks_to_type_or_use_the_menu() {
    let s = setup().await;
    for answer in [json!({}), json!({ "heard": "" }), json!({ "heard": "   " })] {
        let (heard, _) = s.sends(clip(), [Scripted::Json(answer)]).await;
        assert_eq!(reply(heard), s.not_heard().await);
    }
}

/// FR-AI-15: a clip that can't be fetched gets the same reply, and the AI isn't asked.
#[tokio::test]
async fn a_clip_not_received_is_not_sent_to_the_ai() {
    let s = setup().await;
    let oversize = fake::Answer::bytes("audio/x-m4a", &vec![0; MAX_CLIP_BYTES + 1]);
    for served in [
        oversize,
        fake::Answer::error(404, "Not found"),
        fake::Answer::error(410, "Gone"),
        fake::Answer::error(500, "Internal error"),
    ] {
        let (heard, model) = s.sends(served, []).await;
        assert_eq!(reply(heard), s.not_heard().await);
        assert!(model.requests().is_empty());
    }
    assert!(s.downloaded());
}

/// The largest clip is heard all the same.
#[tokio::test]
async fn a_clip_at_the_limit_is_heard() {
    let s = setup().await;
    let served = fake::Answer::bytes("audio/x-m4a", &vec![0; MAX_CLIP_BYTES]);
    let says = [Scripted::Json(json!({ "heard": "ไข่" }))];
    let (heard, model) = s.sends(served, says).await;
    assert_eq!(said(&heard), "ไข่");
    assert_eq!(model.requests().len(), 1);
}

/// FR-AI-10, NFR-REL-06: when the AI fails, the user gets the apology and the menus.
#[tokio::test]
async fn without_an_answer_the_menus_are_offered() {
    let s = setup().await;
    let (heard, _) = s.sends(clip(), [Scripted::Fail]).await;
    assert_eq!(reply(heard), [s.templates.ai_unavailable()]);
}

/// FR-AI-09: at the daily limit the assistant is resting, and the provider isn't called.
#[tokio::test]
async fn at_the_daily_limit_the_assistant_rests() {
    let s = setup().await;
    // All the calls a scripted gateway allows a user in a day
    let (_, key) = daily::keys(Utc::now(), Bangkok, Some(s.user));
    let used = counter::ActiveModel {
        key: Set(key.unwrap()),
        value: Set(1_000),
    };
    used.insert(&s.db).await.unwrap();

    let (heard, model) = s.sends(clip(), []).await;
    assert_eq!(reply(heard), [s.templates.ai_resting()]);
    assert!(model.requests().is_empty());
}

/// FR-ADM-04: a suspended user's clip is neither downloaded nor heard.
#[tokio::test]
async fn a_suspended_user_is_not_heard() {
    let s = setup().await;
    let suspended = user::ActiveModel {
        id: Set(s.user),
        status: Set(user::Status::Suspended),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();

    let (heard, model) = s.sends(clip(), []).await;
    assert_eq!(reply(heard), [s.templates.denied(Denied::Suspended)]);
    assert!(model.requests().is_empty());
    assert!(!s.downloaded());
}
