//! The AI listing assistant (FR-PROD-02, FR-PROD-10, FR-AI-12): a photo and one message drafted
//! into a listing that buyers can't see until "ยืนยันลงขาย" (AC-15, Appendix A.3), the draft
//! checked and moderated as a typed one is, the seller going on step by step without it, a
//! message alone drafting a product too, and quick updates to a price or stock that change
//! nothing until confirmed (FR-UI-13). Each event is handled in a transaction committed once it
//! is handled. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Json, Uuid};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, QueryOrder, TransactionTrait};
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::super::{EDIT, MAX_PRICE, NAME_CHARS, PART, SUBMIT, answer, photo_sent, tapped};
use super::*;
use crate::ai::testing::{self as scripted, Turn as Scripted};
use crate::config::{self, SessionConfig};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Finished, Input, pending};
use crate::line::flex::{Component, FlexContainer, Hero};
use crate::line::testing::{self as fake, Fake};
use crate::line::{Action, Body, limits};
use crate::notifications::templates::{ProductUpdate, Templates, with_choices};
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{ai_call_log, pending_action, session, user};
use crate::persistence::{orm, testing};
use crate::thai::Money;
use crate::users::access::{Actor, Denied};

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../../ai/provider/fixtures/media/slip.jpg");

/// AC-15's message
const KHANOM_KROK: &str = "ขนมครก กล่องละ 20 วันนี้ทำ 30 กล่อง";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

/// What the AI drafts from [`KHANOM_KROK`] (Appendix A.3)
fn khanom_krok() -> Value {
    json!({
        "name": "ขนมครก",
        "price": "20",
        "unit": "กล่อง",
        "category": "ขนม/ของหวาน",
        "stock": "30",
        "description": "ทำสดวันนี้",
    })
}

/// The AI's answer to a seller's message outside a flow: `intent`, with `fields`.
fn read_as(intent: &str, fields: Value) -> Value {
    let mut answer = fields;
    answer["intent"] = json!(intent);
    answer
}

/// The AI asking for a quick update of product `id`, with `fields`.
fn update_of(id: i64, fields: Value) -> Value {
    let mut answer = read_as(UPDATE, fields);
    answer["product"] = json!(id.to_string());
    answer
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    /// The pool [`Setup::db`] is on, for the AI's daily limits
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    /// Off, unless a test scripts it
    ai: ai::Gateway,
    templates: Templates,
    /// The owner of [`Setup::shop`]
    seller: i64,
    /// Approved and open
    shop: i64,
    /// A user without a shop
    buyer: i64,
    /// The active categories, in order: ไข่/นม and ขนม/ของหวาน. ผลไม้ follows, inactive.
    eggs: i64,
    sweets: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool.clone());
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let shop = testing::new_shop(seller).insert(&db).await.unwrap().id;
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    let category = async |name: &str, sort_order: i32, is_active: bool| {
        let category = category::ActiveModel {
            sort_order: Set(sort_order),
            is_active: Set(is_active),
            ..testing::new_category(name)
        };
        category.insert(&db).await.unwrap().id
    };
    let eggs = category("ไข่/นม", 1, true).await;
    let sweets = category("ขนม/ของหวาน", 2, true).await;
    category("ผลไม้", 3, false).await;
    let config = config::sample();
    Setup {
        _container: container,
        pool,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        ai: scripted::off(),
        seller,
        shop,
        buyer,
        eggs,
        sweets,
    }
}

impl Setup {
    /// The AI answering with `turns` in turn, accepting images if `images`, with every feature
    /// on but those in `off`.
    fn script(
        &mut self,
        turns: impl IntoIterator<Item = Scripted>,
        images: bool,
        off: &[Feature],
    ) -> scripted::Model {
        let (ai, model) = scripted::scripted(&self.pool, turns, images, off);
        self.ai = ai;
        model
    }

    /// Handles an event of user `user_id`'s with `handle`, as the worker does: in a transaction
    /// that commits once it is handled, with the user loaded afresh and what it notifies queued.
    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages, outbox, ..
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.config.notification, t0())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        messages
    }

    /// User `user_id` taps a button of "เพิ่มสินค้า" sending `data`.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            tapped(turn, &self.config, &self.ai, &postback).await
        })
        .await
    }

    /// User `user_id` taps the latest preview's button for `action`.
    async fn taps_latest(&self, user_id: i64, action: &str) -> Vec<Message> {
        let revision = self.draft(user_id).await.revision;
        self.taps(user_id, &format!("{action}?r={revision}")).await
    }

    /// User `user_id` taps the confirm button of `message`, its first Quick Reply.
    async fn confirms(&self, user_id: i64, message: &Message) -> Vec<Message> {
        let data = quick_reply_data(message)[0].expect("a confirm button");
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| pending::tapped(turn, &postback).await)
            .await
    }

    /// User `user_id` sends a photo no flow takes, which LINE serves as [`PHOTO`].
    async fn photo(&self, user_id: i64, message_id: &str) -> Vec<Message> {
        self.serves(message_id);
        let line = self.line.client();
        self.handle(user_id, async |turn| {
            photo_sent(turn, &self.config, &self.ai, &line, message_id).await
        })
        .await
    }

    fn serves(&self, message_id: &str) {
        let path = format!("/v2/bot/message/{message_id}/content");
        self.line
            .script(&path, [fake::Answer::bytes("image/jpeg", PHOTO)]);
    }

    /// User `user_id` answers the step of "เพิ่มสินค้า" they are on with `input`.
    async fn sends(&self, user_id: i64, input: Input) -> Vec<Message> {
        let line = self.line.client();
        self.handle(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<ProductFlow>())
                .expect("adding a product");
            let answered = answer(turn, &self.config, &self.ai, &line, session, &input).await?;
            Ok(answered.expect("an answer"))
        })
        .await
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        self.sends(user_id, Input::Text(text.to_owned())).await
    }

    /// User `user_id` types `text` where no flow or keyword takes it (FR-UI-12 step 5); `None`
    /// if it isn't the listing assistant's.
    async fn says(&self, user_id: i64, text: &str) -> Option<Vec<Message>> {
        let mut read = None;
        self.handle(user_id, async |turn| {
            read = typed(turn, &self.config, &self.ai, text).await?.reply();
            Ok(read.clone().unwrap_or_default())
        })
        .await;
        read
    }

    async fn cancels(&self, user_id: i64) {
        self.handle(user_id, async |turn| flow::cancel(turn).await)
            .await;
    }

    /// User `user_id`'s product in progress, if any.
    async fn session(&self, user_id: i64) -> Option<Session<ProductFlow>> {
        let row = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()?;
        Some(Session {
            step: serde_json::from_value(Json::String(row.step)).unwrap(),
            draft: serde_json::from_value(row.data).unwrap(),
        })
    }

    async fn step(&self, user_id: i64) -> Step {
        self.session(user_id).await.expect("adding a product").step
    }

    async fn draft(&self, user_id: i64) -> ProductFlow {
        self.session(user_id).await.expect("adding a product").draft
    }

    async fn products(&self) -> Vec<product::Model> {
        product::Entity::find()
            .order_by_asc(product::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn product(&self, id: i64) -> product::Model {
        product::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Lists `name` at ฿`price` a `unit`, `stock` of them, in ขนม/ของหวาน.
    async fn lists(&self, name: &str, price: i64, unit: &str, stock: Option<i32>) -> i64 {
        let product = product::ActiveModel {
            price: Set(Money::from_baht(price)),
            unit: Set(unit.to_owned()),
            ..testing::new_product(self.shop, self.sweets, name, stock)
        };
        product.insert(&self.db).await.unwrap().id
    }

    /// Sets product `id`'s `column` to `value`, as something else meanwhile would.
    async fn set(&self, id: i64, column: product::Column, value: impl Into<sea_orm::Value>) {
        product::Entity::update_many()
            .col_expr(column, Expr::value(value))
            .filter(product::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// Every image, as (owner type, owner, ID), in their order.
    async fn images(&self) -> Vec<(OwnerType, i64, Uuid)> {
        image::Entity::find()
            .order_by_asc(image::Column::Position)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.owner_type, row.owner_id, row.id))
            .collect()
    }

    async fn pending(&self) -> usize {
        pending_action::Entity::find()
            .all(&self.db)
            .await
            .unwrap()
            .len()
    }

    /// `product`'s card in "สินค้าของฉัน", as it is now.
    async fn card(&self, id: i64) -> Message {
        let product = self.product(id).await;
        let today = t0().with_timezone(&chrono_tz::Asia::Bangkok).date_naive();
        manage::card(&self.db, &self.templates, &self.config, &product, today)
            .await
            .unwrap()
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

/// The postback data of `message`'s Quick Reply buttons; `None` for those that aren't.
fn quick_reply_data(message: &Message) -> Vec<Option<&str>> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| match &item.action {
            Action::Postback { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect()
}

/// The card of `message`.
fn bubble(message: &Message) -> &crate::line::flex::Bubble {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => bubble,
        body => panic!("not a card: {body:?}"),
    }
}

/// The postback data of the buttons of `message`'s card.
fn buttons(message: &Message) -> Vec<String> {
    let Some(footer) = &bubble(message).footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { data, .. } => data.clone(),
                action => panic!("not a postback: {action:?}"),
            },
            component => panic!("not a button: {component:?}"),
        })
        .collect()
}

/// The link of the photo across the top of `message`'s card.
fn hero(message: &Message) -> Option<&str> {
    match &bubble(message).hero {
        Some(Hero::Image(image)) => Some(&image.url),
        _ => None,
    }
}

/// The texts of `message`'s card, in order, the rows' labels and values among them.
fn card_texts(message: &Message) -> Vec<String> {
    fn texts(component: &Component, out: &mut Vec<String>) {
        match component {
            Component::Text(text) => out.push(text.text.clone()),
            Component::Box(flex_box) => {
                for component in &flex_box.contents {
                    texts(component, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for component in &bubble(message).body.as_ref().unwrap().contents {
        texts(component, &mut out);
    }
    out
}

/// The value of the row labelled `label` in `message`'s card.
fn row(message: &Message, label: &str) -> String {
    let texts = card_texts(message);
    let at = texts.iter().position(|text| text == label).expect(label);
    texts[at + 1].clone()
}

/// The JSON Schema the AI's answer was asked to match.
fn schema_of(output: &Output) -> Value {
    match output {
        Output::Json(schema) => schema.to_json(),
        output => panic!("not JSON: {output:?}"),
    }
}

/// The buttons that follow a listed product (Appendix A.3).
fn listed_next() -> Vec<Action> {
    vec![
        Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone()),
        Item::AddProduct
            .postback()
            .button(labels::ADD_ANOTHER.clone()),
    ]
}

/// Where a quick update carries on from: "สินค้าของฉัน" and the menu.
fn carry_on() -> Vec<Action> {
    let mine = Item::MyProducts
        .postback()
        .button(labels::MY_PRODUCTS.clone());
    let menu = Action::message(labels::MENU.clone(), labels::MENU.as_str());
    vec![mine, menu]
}

/// The confirmation of `update`, but for its button's random ID.
fn confirmation(t: &Templates, update: &ProductUpdate<'_>) -> String {
    let confirm = Action::postback(labels::CONFIRM.clone(), "confirm");
    text(&t.confirm_product_update(update, confirm)).to_owned()
}

/// AC-15, Appendix A.3, NFR-USE-02: with the AI on, a photo and *ขนมครก กล่องละ 20 วันนี้ทำ 30
/// กล่อง* draft ฿20 per กล่อง, 30 of them, in ขนม/ของหวาน. Buyers can't see it, as there is no
/// product, until "ยืนยันลงขาย": the one tap that lists it, moderated, with its photo.
#[tokio::test]
async fn a_photo_and_one_message_draft_the_listing() {
    let mut s = setup().await;
    let allow = json!({ "verdict": "allow", "reason": "" });
    let model = s.script(
        [Scripted::Json(khanom_krok()), Scripted::Json(allow)],
        true,
        &[],
    );
    let (a, t) = (s.seller, &s.templates);

    let offered = s.photo(a, "m1").await;
    assert_eq!(offered.len(), 1);
    assert_eq!(text(&offered[0]), text(&t.listing_draft_offered(2)));
    assert!(
        text(&offered[0]).starts_with("ได้รับรูปแล้วค่ะ จะลงขายใช่ไหมคะ พิมพ์ชื่อ ราคา และจำนวนมาได้เลย"),
        "{}",
        text(&offered[0])
    );
    assert_eq!(
        quick_replies(&offered[0]),
        ["ถ่ายรูป", "เลือกรูป", "ถัดไป", "ยกเลิก"]
    );
    let [(OwnerType::Draft, _, photo)] = s.images().await[..] else {
        panic!("one draft: {:?}", s.images().await);
    };
    let session = s.session(a).await.unwrap();
    assert_eq!(
        (session.step, session.draft.ai),
        (Step::Photos, Drafting::Offered)
    );

    let preview = s.types(a, KHANOM_KROK).await;
    assert_eq!(preview.len(), 2);
    assert!(
        text(&preview[0]).starts_with("ร่างสินค้าให้แล้วค่ะ ตรวจดูก่อนนะคะ ถ้าถูกต้อง กด \"ยืนยันลงขาย\""),
        "{}",
        text(&preview[0])
    );
    let card = &preview[1];
    let texts = card_texts(card);
    assert_eq!(texts[..2], ["ขนมครก", "฿20 / กล่อง"]);
    assert!(texts.contains(&"ทำสดวันนี้".to_owned()), "{texts:?}");
    assert_eq!(row(card, "หมวดหมู่"), "ขนม/ของหวาน");
    assert_eq!(row(card, "จำนวน"), "30 กล่อง");
    let photo_url = images::product_image_url(&s.config.app.base_url, photo);
    assert_eq!(hero(card), Some(photo_url.as_str()));
    let draft = s.draft(a).await;
    assert_eq!(
        (s.step(a).await, draft.ai),
        (Step::Summary, Drafting::Drafted)
    );
    let r = draft.revision;
    assert_eq!(
        buttons(card),
        [format!("{SUBMIT}?r={r}"), format!("{EDIT}?r={r}")]
    );
    assert_eq!(quick_replies(card), ["ยกเลิก"]);
    // Nothing a buyer could see
    assert!(s.products().await.is_empty());

    // What the AI was asked, as the seller (FR-AI-06, FR-AI-09, FR-AI-12)
    let requests = model.requests();
    let request = &requests[0];
    assert!(request.system.starts_with(RULES));
    assert!(request.system.contains("Never invent"));
    assert!(request.system.ends_with(ai::DATA_RULE));
    let [Part::Text(asked), Part::Image(_)] = &request.messages[0].parts[..] else {
        panic!(
            "the message, then the photo: {:?}",
            request.messages[0].parts
        );
    };
    assert!(asked.contains("<listing>"), "{asked}");
    assert!(asked.contains(KHANOM_KROK), "{asked}");
    assert!(!asked.contains("<products>"), "{asked}");
    let schema = schema_of(&request.output);
    assert_eq!(
        schema["properties"]["category"]["enum"],
        json!(["ไข่/นม", "ขนม/ของหวาน"])
    );
    assert!(schema["properties"]["intent"].is_null(), "{schema}");
    let calls = ai_call_log::Entity::find()
        .order_by_asc(ai_call_log::Column::Id)
        .all(&s.db)
        .await
        .unwrap();
    assert_eq!(
        (calls[0].feature.as_str(), calls[0].user_id),
        ("listing", Some(a))
    );

    // The one tap
    let listed = s.taps_latest(a, SUBMIT).await;
    assert_eq!(listed, [t.product_listed("ขนมครก", listed_next())]);
    let [product] = &s.products().await[..] else {
        panic!("one product");
    };
    assert_eq!(
        (
            product.name.as_str(),
            product.price,
            product.unit.as_str(),
            product.stock,
            product.category_id,
            product.description.as_deref(),
            product.status,
        ),
        (
            "ขนมครก",
            Money::from_baht(20),
            "กล่อง",
            Some(30),
            s.sweets,
            Some("ทำสดวันนี้"),
            product::Status::Active,
        )
    );
    assert_eq!(s.images().await, [(OwnerType::Product, product.id, photo)]);
    // Moderated, as any other
    assert_eq!(model.requests().len(), 2);
    assert_eq!(s.session(a).await, None);
}

/// FR-PROD-03, FR-AI-05, FR-AI-12: a draft is checked as typed answers are. A category that
/// isn't active is asked for again of the AI; a price or stock out of range is left out and
/// asked for of the seller, then the preview. A model that takes no images isn't sent them.
#[tokio::test]
async fn a_draft_is_checked_as_typed_answers_are() {
    let mut s = setup().await;
    let inactive = json!({ "name": "ขนมครก", "category": "ผลไม้" });
    let out_of_range = json!({
        "name": "ขนมครก",
        "price": "0",
        "unit": "กล่อง",
        "category": "ขนม/ของหวาน",
        "stock": "100000",
        "description": null,
    });
    let model = s.script(
        [Scripted::Json(inactive), Scripted::Json(out_of_range)],
        false,
        &[Feature::Moderation],
    );
    let (a, t) = (s.seller, &s.templates);
    let first = s.taps(a, "add_product").await;
    assert_eq!(text(&first[0]), text(&t.ask_listing_message(0, 3)));
    s.serves("m1");
    s.sends(
        a,
        Input::Image {
            message_id: "m1".to_owned(),
        },
    )
    .await;
    assert_eq!(s.step(a).await, Step::Photos);

    let asked = s.types(a, "ขนมครก กล่องละศูนย์บาท").await;
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "asked again, with a hint");
    assert_eq!(requests[0].messages[0].parts.len(), 1, "no photo");
    assert_eq!(text(&asked[0]), text(&t.listing_drafted_partly()));
    assert_eq!(text(&asked[1]), text(&t.ask_product_price(MAX_PRICE, None)));
    assert_eq!(s.step(a).await, Step::Price);
    let draft = s.draft(a).await;
    assert_eq!(
        (
            draft.name.as_deref(),
            draft.price,
            draft.stock,
            draft.category_id
        ),
        (Some("ขนมครก"), None, None, Some(s.sweets))
    );

    s.types(a, "20").await;
    assert_eq!(s.step(a).await, Step::Stock);
    let preview = s.types(a, "30").await;
    assert!(text(&preview[0]).starts_with("ร่างสินค้าให้แล้วค่ะ"));
    assert_eq!(row(&preview[1], "จำนวน"), "30 กล่อง");
    assert_eq!(card_texts(&preview[1])[1], "฿20 / กล่อง");
    assert!(s.products().await.is_empty());
}

/// FR-AI-10, NFR-REL-06: without a draft the seller goes on step by step from the name, the
/// photo kept: when the AI fails, or finds nothing of a product. With the listing assistant
/// off, the photos step takes photos alone, as before, and the AI isn't asked.
#[tokio::test]
async fn without_a_draft_the_seller_goes_step_by_step() {
    let mut s = setup().await;
    let model = s.script(
        [Scripted::Fail, Scripted::Json(json!({ "name": null }))],
        true,
        &[Feature::Moderation],
    );
    let (a, t) = (s.seller, &s.templates);
    let name = || text(&t.ask_product_name(NAME_CHARS, None)).to_owned();

    s.photo(a, "m1").await;
    let failed = s.types(a, KHANOM_KROK).await;
    assert_eq!(text(&failed[0]), text(&t.listing_not_drafted(false)));
    assert_eq!(text(&failed[1]), name());
    let draft = s.draft(a).await;
    assert_eq!(
        (s.step(a).await, draft.ai, draft.photos.len(), draft.editing),
        (Step::Name, Drafting::Off, 1, false)
    );
    // Step by step from here
    s.types(a, "ขนมครก").await;
    assert_eq!(s.step(a).await, Step::Price);

    s.cancels(a).await;
    s.photo(a, "m2").await;
    let nothing = s.types(a, "สวัสดีค่ะ").await;
    assert_eq!(text(&nothing[0]), text(&t.listing_not_understood()));
    assert_eq!(text(&nothing[1]), name());
    assert_eq!(s.step(a).await, Step::Name);
    assert_eq!(model.requests().len(), 2);

    let model = s.script([], true, &[Feature::Listing]);
    let t = &s.templates;
    s.cancels(a).await;
    let offered = s.photo(a, "m3").await;
    assert_eq!(text(&offered[0]), text(&t.listing_offered(2)));
    let refused = s.types(a, KHANOM_KROK).await;
    assert_eq!(text(&refused[0]), text(&t.photo_expected(&labels::NEXT)));
    assert_eq!(s.step(a).await, Step::Photos);
    assert!(model.requests().is_empty());
    assert!(s.products().await.is_empty());
}

/// FR-PROD-02, FR-UI-04: a drafted preview's "แก้ไข" corrects a part as the guided flow's does,
/// back to the drafted preview; cancelling lists nothing.
#[tokio::test]
async fn a_draft_is_corrected_or_cancelled_as_any_other() {
    let mut s = setup().await;
    s.script(
        [Scripted::Json(khanom_krok())],
        true,
        &[Feature::Moderation],
    );
    let (a, t) = (s.seller, &s.templates);
    s.taps(a, "add_product").await;
    let preview = s.types(a, KHANOM_KROK).await;
    assert_eq!(hero(&preview[1]), None, "no photo sent");

    let choose = s.taps_latest(a, EDIT).await;
    assert!(
        quick_replies(&choose[0]).contains(&labels::PRICE.as_str()),
        "{:?}",
        quick_replies(&choose[0])
    );
    let revision = s.draft(a).await.revision;
    let data = format!("{PART}?p={}&r={revision}", Step::Price.key());
    let price = s.taps(a, &data).await;
    assert_eq!(
        text(&price[0]),
        text(&t.ask_product_price(MAX_PRICE, Some("฿20")))
    );
    let preview = s.types(a, "25").await;
    assert!(text(&preview[0]).starts_with("ร่างสินค้าให้แล้วค่ะ"));
    assert_eq!(card_texts(&preview[1])[1], "฿25 / กล่อง");

    s.cancels(a).await;
    assert_eq!(s.session(a).await, None);
    assert!(s.products().await.is_empty());
}

/// FR-PROD-09: a drafted product is moderated when listed, as any other: a prohibited keyword
/// holds it for review.
#[tokio::test]
async fn a_drafted_listing_is_moderated_as_any_other() {
    let mut s = setup().await;
    s.config.catalog.prohibited_keywords = vec!["กัญชา".to_owned()];
    let cookies = json!({
        "name": "คุกกี้กัญชา",
        "price": "50",
        "unit": "ถุง",
        "category": "ขนม/ของหวาน",
        "stock": "10",
    });
    s.script([Scripted::Json(cookies)], false, &[Feature::Moderation]);
    let (a, t) = (s.seller, &s.templates);
    s.taps(a, "add_product").await;
    s.types(a, "คุกกี้กัญชา ถุงละ 50 มี 10 ถุง").await;
    let held = s.taps_latest(a, SUBMIT).await;
    assert_eq!(held, [t.product_held("คุกกี้กัญชา", listed_next())]);
    let [product] = &s.products().await[..] else {
        panic!("one product");
    };
    assert_eq!(product.status, product::Status::Held);
}

/// FR-PROD-02, FR-UI-12 step 5: a seller's message alone drafts a new product too, in a flow of
/// its own, the AI choosing among the shop's products as data; a message about anything else is
/// left to routing, and nobody else's is read.
#[tokio::test]
async fn a_message_alone_drafts_a_new_product() {
    let mut s = setup().await;
    let eggs = testing::new_product(s.shop, s.eggs, "ไข่ไก่เบอร์ 2", Some(8))
        .insert(&s.db)
        .await
        .unwrap();
    let model = s.script(
        [
            Scripted::Json(read_as(NEW_PRODUCT, khanom_krok())),
            Scripted::Json(read_as(OTHER, json!({}))),
        ],
        true,
        &[Feature::Moderation],
    );
    let a = s.seller;

    let preview = s.says(a, KHANOM_KROK).await.expect("a listing");
    assert!(text(&preview[0]).starts_with("ร่างสินค้าให้แล้วค่ะ"));
    assert_eq!(row(&preview[1], "จำนวน"), "30 กล่อง");
    assert_eq!(hero(&preview[1]), None);
    let draft = s.draft(a).await;
    assert_eq!(
        (s.step(a).await, draft.ai, draft.photos.len()),
        (Step::Summary, Drafting::Drafted, 0)
    );
    let request = &model.requests()[0];
    assert!(request.system.contains(INTENTS), "{}", request.system);
    assert!(request.system.ends_with(ai::DATA_RULE));
    let asked = request.messages[0].text();
    assert!(asked.contains("<products>"), "{asked}");
    assert!(asked.contains("ไข่ไก่เบอร์ 2"), "{asked}");
    let schema = schema_of(&request.output);
    assert_eq!(
        schema["properties"]["intent"]["enum"],
        json!([OTHER, NEW_PRODUCT, UPDATE])
    );
    assert_eq!(
        schema["properties"]["product"]["enum"],
        json!([eggs.id.to_string()])
    );

    s.cancels(a).await;
    assert_eq!(s.says(a, "วันนี้ฝนตกไหมคะ").await, None);
    assert_eq!(s.session(a).await, None);

    // A buyer's message isn't the listing assistant's, nor a suspended seller's
    assert_eq!(s.says(s.buyer, "ขนมครก เหลือ 10").await, None);
    user::Entity::update_many()
        .col_expr(user::Column::Status, Expr::value(user::Status::Suspended))
        .filter(user::Column::Id.eq(a))
        .exec(&s.db)
        .await
        .unwrap();
    assert_eq!(s.says(a, "ขนมครก เหลือ 10").await, None);
    assert_eq!(model.requests().len(), 2);
}

/// FR-PROD-10, FR-UI-13, FR-AI-04: *ขนมครก เหลือ 10* asks to confirm the change, from what the
/// database has, and changes nothing until "ยืนยัน", which saves it once; a price likewise, and
/// a restock makes the product new today (FR-SRCH-09).
#[tokio::test]
async fn a_quick_update_changes_nothing_until_confirmed() {
    let mut s = setup().await;
    let khanom = s.lists("ขนมครก", 20, "กล่อง", Some(30)).await;
    let eggs = s.lists("ไข่ไก่", 110, "แผง", Some(0)).await;
    s.script(
        [
            Scripted::Json(update_of(khanom, json!({ "stock": "10" }))),
            Scripted::Json(update_of(khanom, json!({ "price": "25" }))),
            Scripted::Json(update_of(eggs, json!({ "price": "115", "stock": "12" }))),
        ],
        true,
        &[],
    );
    let (a, t) = (s.seller, &s.templates);

    let asked = s.says(a, "ขนมครก เหลือ 10").await.expect("an update");
    assert_eq!(asked.len(), 1);
    let stock = ProductUpdate {
        name: "ขนมครก",
        unit: "กล่อง",
        price: None,
        stock: Some((Some(30), Some(10))),
    };
    assert_eq!(text(&asked[0]), confirmation(t, &stock));
    assert_eq!(quick_replies(&asked[0]), ["ยืนยัน", "ยกเลิก"]);
    assert_eq!(s.product(khanom).await.stock, Some(30));
    assert_eq!(s.session(a).await, None, "no flow");

    let saved = s.confirms(a, &asked[0]).await;
    let done = t.product_stock_saved(Some(10), "กล่อง", false);
    assert_eq!(saved, with_choices(vec![done], carry_on()));
    let product = s.product(khanom).await;
    assert_eq!(
        (product.stock, product.updated_at, product.restocked_at),
        (Some(10), t0(), None)
    );
    // A second tap changes nothing, even what changed since
    s.set(khanom, product::Column::Stock, 7).await;
    let again = s.confirms(a, &asked[0]).await;
    assert_eq!(again, with_choices(vec![t.product_saved()], carry_on()));
    assert_eq!(s.product(khanom).await.stock, Some(7));

    let asked = s.says(a, "ขนมครก ราคา 25").await.unwrap();
    let price = ProductUpdate {
        name: "ขนมครก",
        unit: "กล่อง",
        price: Some((Money::from_baht(20), Money::from_baht(25))),
        stock: None,
    };
    assert_eq!(text(&asked[0]), confirmation(t, &price));
    let saved = s.confirms(a, &asked[0]).await;
    let done = t.product_price_saved(Money::from_baht(25), "กล่อง");
    assert_eq!(saved, with_choices(vec![done], carry_on()));
    assert_eq!(s.product(khanom).await.price, Money::from_baht(25));

    let asked = s.says(a, "ไข่ไก่ แผงละ 115 มี 12 แผง").await.unwrap();
    let both = ProductUpdate {
        name: "ไข่ไก่",
        unit: "แผง",
        price: Some((Money::from_baht(110), Money::from_baht(115))),
        stock: Some((Some(0), Some(12))),
    };
    assert_eq!(text(&asked[0]), confirmation(t, &both));
    let saved = s.confirms(a, &asked[0]).await;
    assert_eq!(saved, with_choices(vec![t.product_saved()], carry_on()));
    let product = s.product(eggs).await;
    assert_eq!(
        (product.price, product.stock, product.restocked_at),
        (Money::from_baht(115), Some(12), Some(t0()))
    );
}

/// FR-UI-13: a confirmation whose product changed since asks again as it is now; one cancelled
/// or expired changes nothing; one for a product deleted meanwhile is refused.
#[tokio::test]
async fn a_stale_quick_update_shows_the_product_as_it_is() {
    let mut s = setup().await;
    let khanom = s.lists("ขนมครก", 20, "กล่อง", Some(30)).await;
    let to = |stock: &str| Scripted::Json(update_of(khanom, json!({ "stock": stock })));
    s.script([to("10"), to("5"), to("5"), to("5")], true, &[]);
    let (a, t) = (s.seller, &s.templates);

    let asked = s.says(a, "ขนมครก เหลือ 10").await.unwrap();
    // An order takes 2 meanwhile
    s.set(khanom, product::Column::Stock, 28).await;
    let again = s.confirms(a, &asked[0]).await;
    assert_eq!(again.len(), 2);
    assert_eq!(again[0], t.product_update_changed());
    let now = ProductUpdate {
        name: "ขนมครก",
        unit: "กล่อง",
        price: None,
        stock: Some((Some(28), Some(10))),
    };
    assert_eq!(text(&again[1]), confirmation(t, &now));
    assert_eq!(s.product(khanom).await.stock, Some(28));
    s.confirms(a, &again[1]).await;
    assert_eq!(s.product(khanom).await.stock, Some(10));

    let asked = s.says(a, "ขนมครก เหลือ 5").await.unwrap();
    s.cancels(a).await;
    let cancelled = s.confirms(a, &asked[0]).await;
    let not_updated = t.product_not_updated(None);
    assert_eq!(cancelled, with_choices(vec![not_updated], carry_on()));
    assert_eq!(s.product(khanom).await.stock, Some(10));

    let asked = s.says(a, "ขนมครก เหลือ 5").await.unwrap();
    pending_action::Entity::update_many()
        .col_expr(
            pending_action::Column::ExpiresAt,
            Expr::value(t0() - chrono::TimeDelta::minutes(1)),
        )
        .exec(&s.db)
        .await
        .unwrap();
    let expired = s.confirms(a, &asked[0]).await;
    let not_updated = t.product_not_updated(Some("ขนมครก"));
    assert_eq!(expired, with_choices(vec![not_updated], carry_on()));
    assert_eq!(s.product(khanom).await.stock, Some(10));

    let asked = s.says(a, "ขนมครก เหลือ 5").await.unwrap();
    s.set(khanom, product::Column::Status, product::Status::Deleted)
        .await;
    let deleted = s.confirms(a, &asked[0]).await;
    assert_eq!(deleted, [t.denied(Denied::NotFound)]);
    assert_eq!(s.product(khanom).await.stock, Some(10));
}

/// FR-PROD-03, FR-PROD-10: a quick update is checked as "สินค้าของฉัน" checks it, and one for a
/// product the shop hasn't, or that changes nothing, asks nothing to be confirmed.
#[tokio::test]
async fn a_quick_update_proposes_only_a_valid_change() {
    let mut s = setup().await;
    let khanom = s.lists("ขนมครก", 20, "กล่อง", Some(30)).await;
    s.script(
        [
            Scripted::Json(update_of(khanom, json!({ "price": "200000" }))),
            Scripted::Json(read_as(UPDATE, json!({ "product": null, "stock": "10" }))),
            Scripted::Json(update_of(khanom, json!({ "stock": "30", "price": "20" }))),
        ],
        true,
        &[],
    );
    let (a, t) = (s.seller, &s.templates);

    let refused = s.says(a, "ขนมครก ราคา 200000").await.unwrap();
    assert_eq!(
        refused,
        [t.amount_too_large(MAX_PRICE), s.card(khanom).await]
    );

    let missing = s.says(a, "ขนมเทียน เหลือ 10").await.unwrap();
    let mine = Item::MyProducts
        .postback()
        .button(labels::MY_PRODUCTS.clone());
    assert_eq!(missing, [t.quick_update_not_found(mine)]);

    let unchanged = s.says(a, "ขนมครก ราคา 20 มี 30").await.unwrap();
    assert_eq!(unchanged, [t.product_unchanged(), s.card(khanom).await]);

    assert_eq!(s.pending().await, 0);
    let product = s.product(khanom).await;
    assert_eq!(
        (product.price, product.stock),
        (Money::from_baht(20), Some(30))
    );
}
