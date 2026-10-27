//! The AI listing assistant (FR-PROD-02, FR-PROD-10), while `ai.features.listing` is on: a seller
//! lists a product with a photo and one short message, such as *ขนมครก กล่องละ 20 วันนี้ทำ 30
//! กล่อง*, and one tap (Appendix A.3, NFR-USE-02), and changes the price or stock of one in a few
//! words, such as *ขนมครก เหลือ 10* or *ไข่ไก่ ราคา 115*.
//!
//! A message typed where "เพิ่มสินค้า" asks for photos, as it does after a photo no flow took
//! (FR-UI-14), is [`drafted`] from: the AI reads it, with the photos if the model accepts images,
//! and answers with each part of the product as the seller would have typed it at its step. Each
//! part is taken as the flow takes a typed answer, so it is checked as FR-PROD-03 says and its
//! category must be active; one the flow refuses is left out. The flow then asks for any part
//! still missing and shows the preview, from which "ยืนยันลงขาย" lists the product, moderated as
//! any other (FR-PROD-09), "แก้ไข" corrects a part and "ยกเลิก" lists nothing. Nothing is saved
//! before then but the photos, as drafts, so buyers never see a product the seller hasn't
//! confirmed.
//!
//! A seller's message that no flow or keyword took, at routing's step 5 (FR-UI-12), is
//! [`typed`]: the AI tells a new product, drafted the same way, from a change to the price or
//! stock of one of the shop's products, which is an [`UpdateProduct`] to confirm (FR-UI-13), and
//! from anything else, which routing takes on to the assistant (#37).
//!
//! The AI is told to draft only what the seller wrote or the photos show, and to invent nothing
//! (FR-AI-12). It may choose only an active category and only one of the shop's products, and
//! what it answers is validated (FR-AI-05). What the seller wrote, and their products' names, are
//! given to it as data that can't change its rules (FR-AI-06); nothing else of theirs, such as a
//! phone or an address, is sent (FR-AI-08). The call is the seller's, so it counts against their
//! daily limit too (FR-AI-09). Without an answer the seller goes on step by step, or is told the
//! assistant can't help now (FR-AI-10, NFR-REL-06). What the seller types is never logged
//! (NFR-PRIV-05).

#[cfg(test)]
mod tests;

use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde_json::{Value, json};
use tracing::{debug, info};

use super::manage::{self, UpdateProduct};
use super::{
    DESCRIPTION_CHARS, Drafting, NAME_CHARS, ProductFlow, Step, UNIT_CHARS, after, price, prompt,
    stock, take, why,
};
use crate::ai::{self, Call, Feature, Output, Part, Role, Schema, Unavailable};
use crate::catalog::{categories, images};
use crate::config::Config;
use crate::dispatcher::Turn;
use crate::dispatcher::flow::{self, Answer, Session};
use crate::dispatcher::menu::{self, Item};
use crate::line::Message;
use crate::notifications::templates::labels;
use crate::persistence::entity::{category, product};
use crate::users::access::{self, authorize};

/// What a seller's message outside a flow is about, as the AI tells it
const OTHER: &str = "other";
const NEW_PRODUCT: &str = "new_product";
const UPDATE: &str = "update";

/// The most of the shop's products the AI chooses an update's from, those changed last first
const PRODUCTS_FOR_AI: u64 = 100;

/// The parts the AI drafts, in the order the flow asks for them, by their fields in its answer
const PARTS: [(Step, &str); 6] = [
    (Step::Name, "name"),
    (Step::Price, "price"),
    (Step::Unit, "unit"),
    (Step::Category, "category"),
    (Step::Stock, "stock"),
    (Step::Description, "description"),
];

/// What the AI is told of its task; for a message outside a flow, [`INTENTS`] follows, and
/// [`ai::DATA_RULE`] ends it.
const RULES: &str = "\
You help sellers list what they sell on IconMarket: a LINE marketplace where neighbours in one \
Thai village sell home-cooked food, snacks, produce, eggs and meat, household goods, plants, \
crafts and small services to each other. A seller wrote a short message in Thai, perhaps with \
photos of the product. Give the parts of its listing, each as the seller would type it:
- name: what the product is, as the seller calls it, short
- price: the price of one unit in baht, digits only, such as 20 or 12.50
- unit: what one is sold by, such as กล่อง, ถุง, แพ็ค, กิโลกรัม, ขีด, แผง, หวี, กำ, จาน, ชุด or ชิ้น
- stock: how many are for sale now, digits only (\"วันนี้ทำ 30 กล่อง\", \"มี 30\" and \"เหลือ 10\" \
are counts, \"หมด\" is 0), or ทำตามสั่ง when it is made to order or has no limit
- category: one of the categories offered, word for word, when one clearly fits
- description: at most one short Thai sentence, only from what the seller wrote, such as \
\"ทำสดวันนี้\"

Use only what the seller wrote or the photos clearly show. Never invent ingredients, sizes, \
weights, origin, freshness or health claims. Leave a part null when it isn't stated.";

/// How the AI tells what a message outside a flow is about.
const INTENTS: &str = "\
The seller's products are between <products> and </products>. Answer \"intent\":
- \"update\": the message changes the price or stock of one of those products, such as \
\"ขนมครก เหลือ 10\", \"ไข่ไก่ ราคา 115\", or \"วันนี้ทำขนมครก 30 กล่อง\" when ขนมครก is among \
them. Give its id as \"product\", and only the price and stock it changes.
- \"new_product\": the message offers something to sell that isn't among them. Give its parts.
- \"other\": anything else, such as a question, a greeting or an order.";

/// Drafts the product being added in `draft` from `text`, the seller's message where photos are
/// asked for, and its photos: on to the preview, or to the first part still missing. Without
/// the AI's answer, or with nothing of a product in it, the product is asked for step by step.
pub(super) async fn drafted(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    draft: ProductFlow,
    text: &str,
) -> Result<Vec<Message>, DbErr> {
    let categories = categories::active(turn.db).await?;
    let photos = if ai.accepts_images() {
        images::media(turn.db, &draft.photos).await?
    } else {
        Vec::new()
    };
    let message = message(text, &categories, None, photos);
    let answer = interpret(turn, ai, &categories, None, message).await;
    let (session, ask) = match answer {
        Ok(answer) => fill(turn, config, draft, &answer).await?,
        Err(unavailable) => {
            info!(?unavailable, "listing not drafted, asked step by step");
            let resting = unavailable == Unavailable::Resting;
            let why = turn.templates.listing_not_drafted(resting);
            step_by_step(turn, config, draft, why).await?
        }
    };
    flow::answered(turn, Answer::Next(session, ask)).await
}

/// A seller's `text` that no flow or keyword took (FR-UI-12 step 5): a new product, drafted from
/// it, or a change to the price or stock of one of the shop's, to confirm. `None` for anything
/// else, or for someone who can't manage a shop, so routing goes on; they aren't asked about.
pub async fn typed(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    text: &str,
) -> Result<Option<Vec<Message>>, DbErr> {
    if authorize(turn.db, &turn.actor, access::Action::ManageShop)
        .await?
        .is_err()
    {
        return Ok(None);
    }
    let Some(shop) = &turn.actor.shop else {
        return Ok(None);
    };
    let categories = categories::active(turn.db).await?;
    // Those "สินค้าของฉัน" can change
    let products = product::Entity::find()
        .filter(product::Column::ShopId.eq(shop.id))
        .filter(product::Column::Status.is_in([
            product::Status::Active,
            product::Status::Hidden,
            product::Status::Held,
        ]))
        .order_by_desc(product::Column::UpdatedAt)
        .order_by_desc(product::Column::Id)
        .limit(PRODUCTS_FOR_AI)
        .all(turn.db)
        .await?;
    let message = message(text, &categories, Some(&products), Vec::new());
    let answer = match interpret(turn, ai, &categories, Some(&products), message).await {
        Ok(answer) => answer,
        Err(unavailable) => return Ok(Some(vec![unavailable.message(turn.templates)])),
    };
    let intent = answer["intent"].as_str().unwrap_or(OTHER);
    debug!(intent, "a seller's message read");
    match intent {
        NEW_PRODUCT => new_product(turn, config, &categories, &answer)
            .await
            .map(Some),
        UPDATE => update(turn, config, &products, &answer).await.map(Some),
        _ => Ok(None),
    }
}

/// A new product, drafted from `answer`, in a flow of its own; none can be listed while no
/// category is active (FR-PROD-03).
async fn new_product(
    turn: &Turn<'_>,
    config: &Config,
    categories: &[category::Model],
    answer: &Value,
) -> Result<Vec<Message>, DbErr> {
    if categories.is_empty() {
        let none = turn.templates.no_categories();
        return Ok(vec![none, menu::main_menu(&turn.actor)]);
    }
    let (session, ask) = fill(turn, config, ProductFlow::new(Vec::new()), answer).await?;
    flow::start(turn, session, ask).await
}

/// The change `answer` asks for to one of `products`, the shop's, to confirm (FR-PROD-10). Its
/// price and stock are checked as "สินค้าของฉัน" checks them (FR-PROD-03); one refused is
/// explained, with the product's card to change it from.
async fn update(
    turn: &Turn<'_>,
    config: &Config,
    products: &[product::Model],
    answer: &Value,
) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let chosen = part(answer, "product")
        .and_then(|id| products.iter().find(|product| product.id.to_string() == id));
    let Some(product) = chosen else {
        let mine = Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone());
        return Ok(vec![templates.quick_update_not_found(mine)]);
    };
    // Whatever the AI chose, the seller must be allowed to change it (FR-AI-06)
    if let Err(denied) = authorize(
        turn.db,
        &turn.actor,
        access::Action::ManageProduct(product.id),
    )
    .await?
    {
        return Ok(vec![turn.denied(denied)]);
    }
    let new_price = part(answer, "price").map(price).transpose();
    let new_stock = part(answer, "stock")
        .map(|text| stock(text, Some(&product.unit)))
        .transpose();
    let (new_price, new_stock) = match (new_price, new_stock) {
        (Ok(new_price), Ok(new_stock)) => (new_price, new_stock),
        (Err(refused), _) | (_, Err(refused)) => {
            let draft = ProductFlow::from_product(product, Vec::new());
            let why = why(turn, &draft, refused).unwrap_or_else(|| templates.product_unchanged());
            let card = manage::card(turn.db, templates, config, product).await?;
            return Ok(vec![why, card]);
        }
    };
    match UpdateProduct::of(product, new_price, new_stock) {
        Some(update) => Ok(vec![update.propose(turn, product).await?]),
        None => {
            let card = manage::card(turn.db, templates, config, product).await?;
            Ok(vec![templates.product_unchanged(), card])
        }
    }
}

/// Takes the parts `answer` drafts into `draft`, as if typed at their steps, then goes on to the
/// preview, or to the first part still missing. With none of them taken, the product is asked
/// for step by step.
async fn fill(
    turn: &Turn<'_>,
    config: &Config,
    mut draft: ProductFlow,
    answer: &Value,
) -> Result<(Session<ProductFlow>, Vec<Message>), DbErr> {
    let mut taken = 0;
    for (step, field) in PARTS {
        let Some(text) = part(answer, field) else {
            continue;
        };
        // One refused is left out, to be asked for
        if take(turn, &mut draft, step, text).await?.is_ok() {
            taken += 1;
        }
    }
    if taken == 0 {
        info!("nothing of a listing drafted, asked step by step");
        let why = turn.templates.listing_not_understood();
        return step_by_step(turn, config, draft, why).await;
    }
    draft.ai = Drafting::Drafted;
    draft.editing = true;
    draft.revision += 1;
    let step = draft.missing().unwrap_or(Step::Summary);
    info!(
        parts = taken,
        complete = step == Step::Summary,
        "listing drafted"
    );
    let mut ask = Vec::new();
    if step != Step::Summary {
        ask.push(turn.templates.listing_drafted_partly());
    }
    ask.extend(prompt(turn, config, &draft, step).await?);
    Ok((Session { step, draft }, ask))
}

/// The product in `draft` asked for step by step from its name, its photos kept, after `why`.
async fn step_by_step(
    turn: &Turn<'_>,
    config: &Config,
    mut draft: ProductFlow,
    why: Message,
) -> Result<(Session<ProductFlow>, Vec<Message>), DbErr> {
    draft.ai = Drafting::Off;
    draft.revision += 1;
    let step = after(&draft, Step::Photos);
    let mut ask = vec![why];
    ask.extend(prompt(turn, config, &draft, step).await?);
    Ok((Session { step, draft }, ask))
}

/// Asks the AI about the seller's `message` (FR-PROD-02), as the seller: their call counts
/// against their daily limit (FR-AI-09). With `products`, the shop's, it also tells what the
/// message is about. The answer matches the [`schema`] (FR-AI-05).
async fn interpret(
    turn: &Turn<'_>,
    ai: &ai::Gateway,
    categories: &[category::Model],
    products: Option<&[product::Model]>,
    message: ai::Message,
) -> Result<Value, Unavailable> {
    let intents = if products.is_some() {
        format!("\n\n{INTENTS}")
    } else {
        String::new()
    };
    let call = Call {
        feature: Feature::Listing,
        user_id: Some(turn.actor.user.id),
        system: format!("{RULES}{intents}\n\n{}", ai::DATA_RULE),
        messages: vec![message],
        output: Output::Json(schema(categories, products)),
    };
    match ai.complete(call).await {
        Ok(ai::Answer::Json(answer)) => Ok(answer),
        // Never: what asks for JSON is answered with JSON (FR-AI-05)
        Ok(_) => Err(Unavailable::Failed),
        Err(unavailable) => Err(unavailable),
    }
}

/// What the AI is asked: the `categories` it may choose from; for a message outside a flow, the
/// shop's `products`; the seller's `text`; and `photos` after them. The seller's words and
/// products are data (FR-AI-06).
fn message(
    text: &str,
    categories: &[category::Model],
    products: Option<&[product::Model]>,
    photos: Vec<ai::Media>,
) -> ai::Message {
    let names: Vec<&str> = categories
        .iter()
        .map(|category| category.name.as_str())
        .collect();
    let mut paragraphs = vec![format!("Categories offered: {}", json!(names))];
    if let Some(products) = products {
        let products: Vec<Value> = products
            .iter()
            .map(|product| {
                json!({ "id": product.id.to_string(), "name": product.name, "unit": product.unit })
            })
            .collect();
        paragraphs.push(ai::as_data("products", &Value::Array(products)));
    }
    paragraphs.push(ai::as_data("listing", &json!({ "message": text })));
    if !photos.is_empty() {
        paragraphs.push("Its photos follow.".to_owned());
    }
    let parts = std::iter::once(Part::Text(paragraphs.join("\n\n")))
        .chain(photos.into_iter().map(Part::Image))
        .collect();
    ai::Message {
        role: Role::User,
        parts,
    }
}

/// The AI's answer: each part of the product, or null. For a message outside a flow, with the
/// shop's `products`, also what it is about and which product an update changes. A category is
/// one of `categories`, the active ones (FR-AI-12).
fn schema(categories: &[category::Model], products: Option<&[product::Model]>) -> Schema {
    let mut schema = Schema::object();
    if let Some(products) = products {
        // "other" first: the mock answers with it (FR-AIP-05), so chat goes on as without the AI
        let intent = Schema::string()
            .one_of([OTHER, NEW_PRODUCT, UPDATE])
            .describe("what the message is about");
        schema = schema.property("intent", intent);
        if !products.is_empty() {
            let ids = products.iter().map(|product| product.id.to_string());
            let product = Schema::string()
                .one_of(ids)
                .describe("the id of the product an update changes");
            schema = schema.optional("product", product);
        }
    }
    schema = schema
        .optional(
            "name",
            Schema::string()
                .max_length(NAME_CHARS)
                .describe("what the product is"),
        )
        .optional(
            "price",
            Schema::string().describe("baht for one unit, digits only"),
        )
        .optional(
            "unit",
            Schema::string()
                .max_length(UNIT_CHARS)
                .describe("what one is sold by"),
        );
    if !categories.is_empty() {
        let names = categories.iter().map(|category| category.name.clone());
        let category = Schema::string()
            .one_of(names)
            .describe("the category it fits");
        schema = schema.optional("category", category);
    }
    schema
        .optional(
            "stock",
            Schema::string().describe("how many are for sale now, digits only, or ทำตามสั่ง"),
        )
        .optional(
            "description",
            Schema::string()
                .max_length(DESCRIPTION_CHARS)
                .describe("one short sentence from what the seller wrote"),
        )
}

/// The text `answer` gives for `field`, unless it is null or blank.
fn part<'a>(answer: &'a Value, field: &str) -> Option<&'a str> {
    answer[field]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}
