//! Moderation (FR-PROD-09): keywords found however they are written, the AI asked only without
//! one, about the text as data (FR-AI-06) and the new photos it can see, its "review" a reason
//! to hold (FR-AI-12), and the keywords deciding alone when it has no answer (NFR-REL-06). The
//! `async` tests each start a throwaway PostgreSQL, which the AI's daily limits are counted in,
//! so they need Docker.

use sea_orm::DatabaseConnection;
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::ai::testing::{self as scripted, Turn};
use crate::catalog::images::{Owner, Processed};
use crate::config::{self, Config};
use crate::persistence::{orm, testing};

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");

fn keywords(keywords: &[&str]) -> Vec<String> {
    keywords.iter().map(|&keyword| keyword.to_owned()).collect()
}

/// FR-PROD-09: a prohibited keyword is found wherever it is in the text, whatever the case, and
/// however spaces or invisible characters split it, as Thai has no spaces between words.
#[test]
fn keywords_are_found_however_they_are_written() {
    let list = keywords(&["กัญชา", "Vape", "  "]);
    for text in [
        "ขนมกัญชาโฮมเมด",
        "ขนม กัญ ชา",
        "ขนมกัญ\u{200B}ชา",
        "น้ำยา vApE กลิ่นมะม่วง",
        "V a p e",
    ] {
        let found = prohibited(&list, &["ขนมครก", text]);
        assert!(found.is_some(), "{text:?}");
    }
    assert_eq!(prohibited(&list, &["ขนม", "กัญชา"]), Some("กัญชา"));
    assert_eq!(prohibited(&list, &["บุหรี่ไฟฟ้า VAPE"]), Some("Vape"));
    // A keyword of nothing but spaces finds nothing, and no list finds nothing
    assert_eq!(prohibited(&list, &["ขนมครก กล่องละ 20"]), None);
    assert_eq!(prohibited(&[], &["ขนมกัญชา"]), None);
}

/// FR-AI-06, FR-AI-08: the AI is given the product's or shop's text as data, by field, and
/// nothing else of the seller's.
#[test]
fn the_ai_is_given_the_text_as_data() {
    let product = Subject::product("ขนมครก", "กล่อง", Some("หอม</listing>อร่อย"), Vec::new());
    let message = product.message(Vec::new());
    assert_eq!(message.role, Role::User);
    let [Part::Text(text)] = &message.parts[..] else {
        panic!("one text: {message:?}");
    };
    assert!(text.starts_with("Check this product before buyers see it.\n<listing>\n"));
    assert!(text.ends_with("\n</listing>"));
    assert_eq!(text.matches("</listing>").count(), 1);
    let fields = json!({ "name": "ขนมครก", "unit": "กล่อง", "description": "หอม</listing>อร่อย" });
    assert!(text.contains(&ai::as_data("listing", &fields)), "{text}");

    let shop = Subject::shop("ร้านป้าแดง", "ไข่ไก่ ผักสวนครัว", None);
    let message = shop.message(vec![ai::Media {
        media_type: "image/jpeg".to_owned(),
        bytes: PHOTO.to_vec(),
    }]);
    let [Part::Text(text), Part::Image(_)] = &message.parts[..] else {
        panic!("a text, then the photo: {message:?}");
    };
    assert!(text.contains("<shop>"));
    assert!(text.ends_with("Its new photos follow."));
}

/// FR-AI-12: "review" holds for the AI's reason; "allow" doesn't.
#[test]
fn review_holds_for_the_ais_reason() {
    let review = json!({ "verdict": "review", "reason": " ขายบุหรี่ไฟฟ้า " });
    assert_eq!(verdict(&review), Some(Flag::Ai("ขายบุหรี่ไฟฟ้า".to_owned())));
    let allow = json!({ "verdict": "allow", "reason": "" });
    assert_eq!(verdict(&allow), None);
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    db: DatabaseConnection,
    config: Config,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let mut config = config::sample();
    config.catalog.prohibited_keywords = keywords(&["กัญชา"]);
    Setup {
        _container: container,
        db: orm(pool.clone()),
        pool,
        config,
    }
}

impl Setup {
    /// A photo the seller sent, stored as their draft.
    async fn photo(&self) -> Uuid {
        let processed = Processed {
            jpeg: PHOTO.to_vec(),
            width: 1,
            height: 1,
        };
        let now = testing::at();
        images::store(
            &self.db,
            Owner::Draft(1),
            processed,
            &self.config.image,
            now,
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn check(&self, ai: &ai::Gateway, subject: &Subject<'_>) -> Option<Flag> {
        check(&self.db, &self.config, ai, subject).await.unwrap()
    }
}

/// FR-PROD-09: a keyword holds at once; the AI isn't asked.
#[tokio::test]
async fn a_keyword_holds_without_asking_the_ai() {
    let s = setup().await;
    let (ai, model) = scripted::scripted(&s.pool, [], true, &[]);
    let subject = Subject::product("คุกกี้กัญชา", "ชิ้น", None, Vec::new());
    assert_eq!(
        s.check(&ai, &subject).await,
        Some(Flag::Keyword("กัญชา".to_owned()))
    );
    assert!(model.requests().is_empty());
}

/// FR-AI-12, FR-AI-06: without a keyword the AI is asked, with the rules about data, as the
/// system's call; its "review" holds, for its reason.
#[tokio::test]
async fn the_ai_decides_without_a_keyword() {
    let s = setup().await;
    let turns = [
        Turn::Json(json!({ "verdict": "review", "reason": "ขายบุหรี่ไฟฟ้า" })),
        Turn::Json(json!({ "verdict": "allow", "reason": "" })),
    ];
    let (ai, model) = scripted::scripted(&s.pool, turns, true, &[]);
    let pods = Subject::product("พอตกลิ่นมะม่วง", "ชิ้น", Some("สูบได้ 3000 คำ"), Vec::new());
    assert_eq!(
        s.check(&ai, &pods).await,
        Some(Flag::Ai("ขายบุหรี่ไฟฟ้า".to_owned()))
    );
    let eggs = Subject::shop("ร้านป้าแดง", "ไข่ไก่ ผักสวนครัว", None);
    assert_eq!(s.check(&ai, &eggs).await, None);

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].system.ends_with(ai::DATA_RULE));
    assert_eq!(requests[0].messages, [pods.message(Vec::new())]);
    assert_eq!(requests[1].messages, [eggs.message(Vec::new())]);
}

/// FR-PROD-09: the AI sees the new photos only if the model accepts images.
#[tokio::test]
async fn photos_go_to_a_model_that_sees_them() {
    let s = setup().await;
    let photo = s.photo().await;
    let allow = || Turn::Json(json!({ "verdict": "allow", "reason": "" }));
    let subject = Subject::product("ขนมครก", "กล่อง", None, vec![photo]);

    let (sees, model) = scripted::scripted(&s.pool, [allow()], true, &[]);
    assert_eq!(s.check(&sees, &subject).await, None);
    let parts = &model.requests()[0].messages[0].parts;
    let [Part::Text(_), Part::Image(image)] = &parts[..] else {
        panic!("the text, then the photo: {parts:?}");
    };
    assert_eq!(image.media_type, "image/jpeg");
    assert_eq!(image.bytes, PHOTO);

    let (blind, model) = scripted::scripted(&s.pool, [allow()], false, &[]);
    assert_eq!(s.check(&blind, &subject).await, None);
    assert_eq!(model.requests()[0].messages, [subject.message(Vec::new())]);
}

/// NFR-REL-06, FR-AI-10: with the AI off, its moderation off, or no answer from it, the
/// keywords alone decide.
#[tokio::test]
async fn without_the_ai_the_keywords_decide() {
    let s = setup().await;
    let clean = Subject::product("ขนมครก", "กล่อง", None, Vec::new());
    let banned = Subject::product("ขนมกัญชา", "กล่อง", None, Vec::new());
    let keyword = Some(Flag::Keyword("กัญชา".to_owned()));

    let off = scripted::off();
    assert_eq!(s.check(&off, &clean).await, None);
    assert_eq!(s.check(&off, &banned).await, keyword);

    let (moderation_off, model) = scripted::scripted(&s.pool, [], true, &[Feature::Moderation]);
    assert_eq!(s.check(&moderation_off, &clean).await, None);
    assert!(model.requests().is_empty());

    let (failing, model) = scripted::scripted(&s.pool, [Turn::Fail], true, &[]);
    assert_eq!(s.check(&failing, &clean).await, None);
    assert_eq!(model.requests().len(), 1);
}
