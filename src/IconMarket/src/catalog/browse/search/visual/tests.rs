//! Visual search (FR-SRCH-10): a buyer's photo searched by what the AI sees in it, with what is
//! found from the database alone and paged as any search's results; the keyword asked for when
//! the AI sees nothing to search for; the photo never stored; and nothing asked of the AI for a
//! photo not received or a suspended buyer. Each event is handled in a transaction committed
//! once it is handled. The tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::TimeDelta;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryOrder, TransactionTrait,
};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::super::SearchFlow;
use super::*;
use crate::ai::testing::{self as scripted, Model, Turn as Scripted};
use crate::catalog::browse;
use crate::config::{self, SessionConfig};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Finished, Input};
use crate::line::flex::{Bubble, Component, FlexContainer};
use crate::line::testing::{self as fake, Fake};
use crate::line::{Action, Body, limits};
use crate::notifications::templates::{Browsed, PRODUCTS_PER_PAGE, Templates, labels};
use crate::persistence::entity::{ai_call_log, image, product, session, shop, user};
use crate::persistence::{orm, testing};
use crate::thai::Money;
use crate::users::access::{Actor, Denied};

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";

/// The message of the photo the buyer sends, and the path LINE serves it at
const PHOTO_ID: &str = "p1";
const CONTENT: &str = "/v2/bot/message/p1/content";
/// The JPEG LINE serves as the photo
const PHOTO: &[u8] = include_bytes!("../../../../ai/provider/fixtures/media/slip.jpg");

/// When the buyer sends the photo: three days after the products [`testing::new_product`] makes
/// were listed
fn now() -> DateTimeUtc {
    testing::at() + TimeDelta::days(3)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    templates: Templates,
    buyer: i64,
    /// ร้านป้าแดง, approved and open
    shop: i64,
    /// The active categories, in order: ไข่/นม and ขนม/ของหวาน
    eggs: i64,
    sweets: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool.clone());
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let shop = testing::new_shop(seller).insert(&db).await.unwrap().id;
    let eggs = testing::new_category("ไข่/นม").insert(&db).await.unwrap().id;
    let sweets = category::ActiveModel {
        sort_order: Set(2),
        ..testing::new_category("ขนม/ของหวาน")
    };
    let sweets = sweets.insert(&db).await.unwrap().id;
    let config = config::sample();
    Setup {
        _container: container,
        pool,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        buyer,
        shop,
        eggs,
        sweets,
    }
}

impl Setup {
    /// Lists a product called `name` of `shop`'s in category `category_id`, for `baht` a unit.
    async fn list_in(&self, shop: i64, category_id: i64, name: &str, baht: i64) {
        let product = product::ActiveModel {
            price: Set(Money::from_baht(baht)),
            ..testing::new_product(shop, category_id, name, Some(5))
        };
        product.insert(&self.db).await.unwrap();
    }

    /// Lists a product of ร้านป้าแดง's.
    async fn list(&self, category_id: i64, name: &str, baht: i64) {
        self.list_in(self.shop, category_id, name, baht).await;
    }

    /// Handles an event of the buyer's with `handle`, as the worker does. Returns the reply,
    /// which must fit LINE's limits.
    async fn handle(
        &self,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, self.buyer)
            .await
            .unwrap()
            .expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, now())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished { messages, .. } = turn.finish(messages);
        txn.commit().await.unwrap();
        limits::check(&messages).unwrap();
        messages
    }

    /// The buyer sends a photo that nothing else takes, which LINE serves as `served`, while
    /// the AI accepts images and answers with `says`: the reply, and the model, which knows
    /// what it was asked.
    async fn sends_photo(
        &self,
        served: fake::Answer,
        says: impl IntoIterator<Item = Scripted>,
    ) -> (Vec<Message>, Model) {
        self.line.script(CONTENT, [served]);
        let (ai, model) = scripted::scripted(&self.pool, says, true, &[]);
        let line = self.line.client();
        let reply = self
            .handle(async |turn| photo_sent(turn, &self.config, &ai, &line, PHOTO_ID).await)
            .await;
        (reply, model)
    }

    /// The buyer sends the photo, and the AI sees in it `seen`.
    async fn searches_by(&self, seen: Value) -> (Vec<Message>, Model) {
        let photo = fake::Answer::bytes("image/jpeg", PHOTO);
        self.sends_photo(photo, [Scripted::Json(seen)]).await
    }

    /// The buyer taps a button sending `data`.
    async fn taps(&self, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle(async |turn| browse::tapped(turn, &self.config, &postback).await)
            .await
    }

    /// The buyer types `text` in the search they are in.
    async fn types(&self, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.handle(async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<SearchFlow>());
            let session = session.expect("searching");
            let answered = super::super::answer(turn, &self.config, session, &input).await?;
            Ok(answered.expect("an answer"))
        })
        .await
    }

    /// The flow the buyer is in, if any.
    async fn flow(&self) -> Option<String> {
        session::Entity::find_by_id(self.buyer)
            .one(&self.db)
            .await
            .unwrap()
            .map(|row| row.flow)
    }

    /// Whether LINE was asked for the photo
    fn downloaded(&self) -> bool {
        self.line
            .requests()
            .iter()
            .any(|request| request.path == CONTENT)
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The labels of `message`'s Quick Reply buttons.
fn quick_replies(message: &Message) -> Vec<&str> {
    let Some(quick_reply) = &message.quick_reply else {
        return Vec::new();
    };
    quick_reply
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
}

/// The texts of each product card of a page of a list, in order: its name, price, shop and
/// availability; and the postback data of its "ดูเพิ่มเติม" if any.
fn page(message: &Message) -> (Vec<Vec<String>>, Option<String>) {
    let cards: Vec<&Bubble> = match &message.body {
        Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } => carousel.contents.iter().collect(),
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => vec![bubble],
        body => panic!("not a card: {body:?}"),
    };
    let mut products = Vec::new();
    let mut more = None;
    for card in cards {
        let contents = &card.body.as_ref().unwrap().contents;
        if let [Component::Button(button)] = contents.as_slice()
            && let Action::Postback { label, data, .. } = &button.action
            && *label == labels::MORE
        {
            more = Some(data.clone());
            continue;
        }
        products.push(texts(contents));
    }
    (products, more)
}

fn texts(contents: &[Component]) -> Vec<String> {
    let mut out = Vec::new();
    for component in contents {
        match component {
            Component::Text(text) => out.push(text.text.clone()),
            Component::Box(flex_box) => out.extend(texts(&flex_box.contents)),
            _ => {}
        }
    }
    out
}

/// The names of the products on a page of a list.
fn names(message: &Message) -> Vec<String> {
    let (products, _) = page(message);
    products.into_iter().map(|texts| texts[0].clone()).collect()
}

/// Acceptance, FR-SRCH-10: what the AI sees in the photo is searched as a typed keyword is, in
/// the category it fits, and what is found, prices and stock included, is the database's. The
/// photo goes to the AI as the buyer's call, and is never stored.
#[tokio::test]
async fn a_photo_is_searched_by_what_the_ai_sees_in_it() {
    let s = setup().await;
    s.list(s.sweets, "ขนมครก", 20).await;
    s.list(s.sweets, "ขนมถ้วยฟู", 15).await;
    // Not in the category the AI chose, nor something it named
    s.list(s.eggs, "ขนมครก แช่แข็ง", 50).await;
    s.list(s.sweets, "ทองหยิบ", 30).await;
    // Of a shop buyers don't see
    let owner = testing::new_user("U-pending").insert(&s.db).await.unwrap();
    let pending = shop::ActiveModel {
        status: Set(shop::Status::Pending),
        ..testing::new_shop(owner.id)
    };
    let pending = pending.insert(&s.db).await.unwrap().id;
    s.list_in(pending, s.sweets, "ขนมครก ป้าศรี", 10).await;

    let seen = json!({ "keywords": ["ขนมครก", "ขนมถ้วย"], "category": "ขนม/ของหวาน" });
    let (reply, model) = s.searches_by(seen).await;
    let [searched, found] = &reply[..] else {
        panic!("not two messages: {reply:?}");
    };
    assert_eq!(searched, &s.templates.photo_searched("ขนมครก"));
    let (mut products, more) = page(found);
    products.sort();
    assert_eq!(
        products,
        [
            ["ขนมครก", "฿20 / แผง", "ร้านป้าแดง", "เหลือ 5"],
            ["ขนมถ้วยฟู", "฿15 / แผง", "ร้านป้าแดง", "เหลือ 5"],
        ]
    );
    assert_eq!(more, None);

    // What the AI was asked, as the buyer (FR-AI-06, FR-AI-09)
    let requests = model.requests();
    let [request] = &requests[..] else {
        panic!("not one request: {requests:?}");
    };
    assert!(request.system.starts_with(RULES));
    assert!(request.system.ends_with(ai::DATA_RULE));
    let [Part::Text(asked), Part::Image(photo)] = &request.messages[0].parts[..] else {
        panic!(
            "the categories, then the photo: {:?}",
            request.messages[0].parts
        );
    };
    assert!(asked.contains(r#"["ไข่/นม","ขนม/ของหวาน"]"#), "{asked}");
    assert_eq!(photo.media_type, "image/jpeg");
    let Output::Json(schema) = &request.output else {
        panic!("not JSON: {:?}", request.output);
    };
    let schema = schema.to_json();
    assert_eq!(
        schema["properties"]["keywords"]["maxItems"],
        json!(KEYWORDS)
    );
    assert_eq!(
        schema["properties"]["category"]["enum"],
        json!(["ไข่/นม", "ขนม/ของหวาน"])
    );
    let calls = ai_call_log::Entity::find()
        .order_by_asc(ai_call_log::Column::Id)
        .all(&s.db)
        .await
        .unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| (call.feature.as_str(), call.user_id))
            .collect::<Vec<_>>(),
        [("visual_search", Some(s.buyer))]
    );

    // Loading while the AI looks (FR-LINE-08), and the photo kept nowhere
    let paths: Vec<String> = s.line.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [CONTENT, "/v2/bot/chat/loading/start"]);
    assert_eq!(image::Entity::find().count(&s.db).await.unwrap(), 0);
}

/// FR-SRCH-03, FR-SRCH-08: what a photo finds is paged as a typed search's is, and when nothing
/// matches the reply says so and suggests where else to look, typing a keyword included.
#[tokio::test]
async fn what_a_photo_finds_is_paged_and_nothing_found_says_so() {
    let s = setup().await;
    for number in 1..=11 {
        s.list(s.eggs, &format!("ไข่ไก่ {number}"), 110).await;
    }

    let (reply, _) = s.searches_by(json!({ "keywords": ["ไข่ไก่"] })).await;
    assert_eq!(reply[0], s.templates.photo_searched("ไข่ไก่"));
    let (products, more) = page(&reply[1]);
    assert_eq!(products.len(), PRODUCTS_PER_PAGE);
    let more = more.expect("ดูเพิ่มเติม");
    let second = s.taps(&more).await;
    assert_eq!(names(&second[0]).len(), 1);

    let (reply, _) = s.searches_by(json!({ "keywords": ["ทุเรียน"] })).await;
    let [searched, none] = &reply[..] else {
        panic!("not two messages: {reply:?}");
    };
    assert_eq!(searched, &s.templates.photo_searched("ทุเรียน"));
    let expected = s
        .templates
        .nothing_found(Browsed::Search("ทุเรียน"), Vec::new());
    assert_eq!(text(none), text(&expected));
    assert_eq!(
        quick_replies(none),
        [
            "ค้นหาใหม่",
            "ของใหม่วันนี้",
            "ร้านค้าทั้งหมด",
            "ไข่/นม",
            "ขนม/ของหวาน",
            "เมนู"
        ]
    );
}

/// A photo of nothing the market could sell: the keyword is asked for, and what the buyer types
/// next is searched.
#[tokio::test]
async fn a_photo_of_nothing_to_search_for_asks_for_the_keyword() {
    let s = setup().await;
    s.list(s.sweets, "ขนมครก", 20).await;

    let (reply, _) = s.searches_by(json!({ "keywords": [] })).await;
    let [asked] = &reply[..] else {
        panic!("not one message: {reply:?}");
    };
    assert_eq!(text(asked), text(&s.templates.photo_not_recognized()));
    assert_eq!(quick_replies(asked), ["ยกเลิก"]);
    assert_eq!(s.flow().await.as_deref(), Some("search"));

    // A blank keyword is none either
    let (reply, _) = s.searches_by(json!({ "keywords": [" ", ""] })).await;
    assert_eq!(text(&reply[0]), text(&s.templates.photo_not_recognized()));

    let found = s.types("ขนมครก").await;
    assert_eq!(names(&found[0]), ["ขนมครก"]);
    assert_eq!(s.flow().await, None);
}

/// FR-AI-10: without an answer the buyer is told the assistant can't help now, with the menus.
#[tokio::test]
async fn without_an_answer_the_buyer_is_told() {
    let s = setup().await;
    let photo = fake::Answer::bytes("image/jpeg", PHOTO);
    let (reply, _) = s.sends_photo(photo, [Scripted::Fail]).await;
    assert_eq!(reply, [s.templates.ai_unavailable()]);
}

/// FR-PROD-04, FR-LINE-09: a photo that can't be received isn't searched by, and the buyer is
/// told what to send instead.
#[tokio::test]
async fn a_photo_not_received_is_not_searched_by() {
    let s = setup().await;
    let actor = Actor::load(&s.db, s.buyer).await.unwrap().unwrap();
    let main_menu = menu::main_menu(&actor);
    for (served, rejected) in [
        (
            fake::Answer::bytes("text/plain", b"not a photo"),
            Rejected::NotJpegOrPng,
        ),
        (fake::Answer::error(404, "Not found"), Rejected::Unavailable),
    ] {
        let seen = Scripted::Json(json!({ "keywords": ["ขนมครก"] }));
        let (reply, model) = s.sends_photo(served, [seen]).await;
        let told = Message::text(s.templates.image_rejected(rejected));
        assert_eq!(reply, [told, main_menu.clone()]);
        assert!(model.requests().is_empty());
    }
}

/// FR-ADM-04: a suspended buyer is told why, and the photo is neither downloaded nor sent to
/// the AI.
#[tokio::test]
async fn a_suspended_buyer_cant_search_by_photo() {
    let s = setup().await;
    let suspended = user::ActiveModel {
        id: Set(s.buyer),
        status: Set(user::Status::Suspended),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();
    let (reply, model) = s.searches_by(json!({ "keywords": ["ขนมครก"] })).await;
    assert_eq!(reply, [s.templates.denied(Denied::Suspended)]);
    assert!(model.requests().is_empty());
    assert!(!s.downloaded());
}
