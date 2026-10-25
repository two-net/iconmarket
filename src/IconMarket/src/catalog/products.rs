//! Products (FR-PROD): adding one ("เพิ่มสินค้า"), and whether it can be ordered. Built by #25
//! and #26; #24 adds what a buyer can do with a product while its shop is closed.
//!
//! Adding a product is a [flow](ProductFlow) that asks for its photos, name, price, unit,
//! category, stock and description in turn, each checked as FR-PROD-03 says, then shows it as a
//! preview (FR-PROD-01). The preview's buttons list it ("ยืนยันลงขาย") or correct one part; they
//! carry the draft's revision, so only the latest preview acts (FR-UI-13). Nothing is saved until
//! it is listed but its photos, which are downloaded as they arrive (FR-LINE-09) and kept as
//! [drafts](Owner::Draft) of the seller's until then, so buyers never see a product before
//! "ยืนยันลงขาย" is tapped. The flow takes a photo at any step, as one more of the product's;
//! a seller's photo that no flow takes starts one with it (FR-UI-14). What the user types is
//! never logged (NFR-PRIV-05).
//!
//! A listed product is `ACTIVE`; [`may_become`] says which status it can go to from there
//! (FR-PROD-07). Its price and stock are what new checkouts see: an order keeps the name, unit and
//! price it was placed at (FR-PROD-08, DR-03).
//!
//! A product's [`Availability`] is what its card says of it (FR-SRCH-07): how many are left, or
//! that it is made to order, sold out (FR-PROD-06) or of a closed shop (FR-SHOP-05). Only an
//! orderable one can go in a cart (#29) or be checked out (#32).

#![allow(
    dead_code,
    reason = "the product card (#28), the cart (#29), checkout (#32) and \"สินค้าของฉัน\" (#26) \
              are built by later issues"
)]

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::images::{self, Owner, Rejected};
use super::{categories, shops};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::{self, Item};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{self, Action, Message, limits};
use crate::notifications::templates::{self, ProductPreview, labels, with_choices};
use crate::persistence::entity::{category, product, shop};
use crate::thai::{self, Label, Money, NumberError};
use crate::users::access::{self, Denied, authorize};

/// What the actions of the flow's buttons start with
pub const PREFIX: &str = "add_product.";
/// The actions of the preview's buttons, which carry the draft's revision in `r`: list the
/// product, or ask which part to correct
pub const SUBMIT: &str = "add_product.submit";
pub const EDIT: &str = "add_product.edit";
/// Correct the part named in `p`
pub const PART: &str = "add_product.part";
/// The categories' Quick Replies from `page` on
pub const CATEGORIES: &str = "add_product.categories";
const REVISION: &str = "r";
const WHICH: &str = "p";
const PAGE: &str = "page";

/// FR-PROD-03
pub const NAME_CHARS: usize = 80;
pub const UNIT_CHARS: usize = 20;
pub const DESCRIPTION_CHARS: usize = 500;
pub const MIN_PRICE: Money = Money::from_satang(1);
pub const MAX_PRICE: Money = Money::from_baht(100_000);
pub const MAX_STOCK: u32 = 99_999;

/// A product being added (FR-PROD-01). A part left `None` once its step is past means "none":
/// no description.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductFlow {
    /// Draft images of the seller's, in the order sent: at most `image.maxPerProduct`
    photos: Vec<Uuid>,
    name: Option<String>,
    price: Option<Money>,
    unit: Option<String>,
    category_id: Option<i64>,
    stock: Option<Stock>,
    description: Option<String>,
    /// Whether every part has been given, so that each answer leads back to the preview
    editing: bool,
    /// How many answers were taken, so that a preview's buttons act only on what it showed
    revision: i64,
}

/// How many of a product there are for new orders (FR-PROD-03, FR-PROD-05).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stock {
    /// 0 – [`MAX_STOCK`]; 0 is sold out (FR-PROD-06)
    Count(i32),
    /// "ไม่จำกัด / ทำตามสั่ง"
    Unlimited,
}

impl Stock {
    /// As `products.stock` keeps it: `None` is unlimited or made to order.
    pub fn column(self) -> Option<i32> {
        match self {
            Self::Count(count) => Some(count),
            Self::Unlimited => None,
        }
    }
}

/// The steps, in the order they are asked. Each but the preview is also a part the preview can
/// correct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Photos,
    Name,
    Price,
    Unit,
    Category,
    Stock,
    Description,
    /// The preview, with "ยืนยันลงขาย"
    Summary,
}

impl Flow for ProductFlow {
    const NAME: &'static str = "add_product";
    type Step = Step;
}

impl Step {
    /// The parts, in the order the preview offers them.
    const PARTS: [Self; 7] = [
        Self::Photos,
        Self::Name,
        Self::Price,
        Self::Unit,
        Self::Category,
        Self::Stock,
        Self::Description,
    ];

    /// Its name in the postback correcting it.
    fn key(self) -> &'static str {
        match self {
            Self::Photos => "photos",
            Self::Name => "name",
            Self::Price => "price",
            Self::Unit => "unit",
            Self::Category => "category",
            Self::Stock => "stock",
            Self::Description => "description",
            Self::Summary => "summary",
        }
    }

    fn label(self) -> &'static Label {
        match self {
            Self::Photos => &labels::PRODUCT_PHOTOS,
            Self::Name => &labels::PRODUCT_NAME,
            Self::Price => &labels::PRICE,
            Self::Unit => &labels::UNIT,
            Self::Category => &labels::CATEGORIES,
            Self::Stock => &labels::STOCK,
            Self::Description => &labels::PRODUCT_DESCRIPTION,
            Self::Summary => &labels::EDIT,
        }
    }

    fn part(key: &str) -> Option<Self> {
        Self::PARTS.into_iter().find(|step| step.key() == key)
    }
}

impl ProductFlow {
    /// A new product, with `photos` and nothing else given yet.
    fn new(photos: Vec<Uuid>) -> Self {
        Self {
            photos,
            name: None,
            price: None,
            unit: None,
            category_id: None,
            stock: None,
            description: None,
            editing: false,
            revision: 0,
        }
    }

    /// The first part the product can't do without, if one is missing: all but its photos and
    /// description.
    fn missing(&self) -> Option<Step> {
        if self.name.is_none() {
            Some(Step::Name)
        } else if self.price.is_none() {
            Some(Step::Price)
        } else if self.unit.is_none() {
            Some(Step::Unit)
        } else if self.category_id.is_none() {
            Some(Step::Category)
        } else if self.stock.is_none() {
            Some(Step::Stock)
        } else {
            None
        }
    }

    /// Whether `step` has a value to keep while the product is being corrected.
    fn has(&self, step: Step) -> bool {
        match step {
            Step::Photos => !self.photos.is_empty(),
            Step::Name => self.name.is_some(),
            Step::Price => self.price.is_some(),
            Step::Unit => self.unit.is_some(),
            Step::Category => self.category_id.is_some(),
            Step::Stock => self.stock.is_some(),
            Step::Description => self.description.is_some(),
            Step::Summary => false,
        }
    }
}

/// The step after `step` was answered: the next one in order, or, while correcting, back to the
/// preview; or the first part still missing.
fn after(draft: &ProductFlow, step: Step) -> Step {
    let next = if draft.editing {
        Step::Summary
    } else {
        match step {
            Step::Photos => Step::Name,
            Step::Name => Step::Price,
            Step::Price => Step::Unit,
            Step::Unit => Step::Category,
            Step::Category => Step::Stock,
            Step::Stock => Step::Description,
            Step::Description | Step::Summary => Step::Summary,
        }
    };
    match next {
        Step::Summary => draft.missing().unwrap_or(Step::Summary),
        next => next,
    }
}

/// How many photos a product may have (FR-PROD-04).
fn max_photos(config: &Config) -> usize {
    usize::try_from(config.image.max_per_product).unwrap_or(usize::MAX)
}

/// A tap on "เพิ่มสินค้า", or on one of the flow's buttons: only for the owner of an approved
/// shop (FR-SHOP-10). A button of a preview that is not the latest asks the current step again;
/// one of a flow no longer in progress offers to start another, and never lists anything.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let Some(shop) = &turn.actor.shop else {
        return Ok(vec![turn.templates.denied(Denied::NotAllowed)]);
    };
    let session = turn
        .flow
        .as_ref()
        .and_then(|flow| flow.read::<ProductFlow>());
    let action = postback.action();
    if action == Item::AddProduct.action() {
        return match session {
            Some(session) => again(turn, config, session).await,
            None => start(turn, config).await,
        };
    }
    let Some(session) = session else {
        return Ok(vec![gone(turn)]);
    };
    if action == CATEGORIES {
        if session.step != Step::Category {
            return again(turn, config, session).await;
        }
        let page = postback
            .int(PAGE)
            .and_then(|page| usize::try_from(page).ok())
            .unwrap_or(0);
        let ask = ask_category(turn, config, &session.draft, page).await?;
        return flow::answered(turn, Answer::<ProductFlow>::Again(ask)).await;
    }
    let latest =
        session.step == Step::Summary && postback.int(REVISION) == Some(session.draft.revision);
    if !latest {
        return again(turn, config, session).await;
    }
    match (action, postback.name(WHICH).and_then(Step::part)) {
        (SUBMIT, _) => submit(turn, config, shop, session.draft).await,
        (EDIT, _) => {
            let choose = turn.templates.choose_part(parts(&session.draft));
            flow::answered(turn, Answer::<ProductFlow>::Again(vec![choose])).await
        }
        (PART, Some(part)) => {
            let ask = prompt(turn, config, &session.draft, part).await?;
            let session = Session {
                step: part,
                draft: session.draft,
            };
            flow::answered(turn, Answer::Next(session, ask)).await
        }
        _ => again(turn, config, session).await,
    }
}

/// A new product, from its photos on; none can be listed while there is no active category to
/// put it in (FR-PROD-03).
async fn start(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    if categories::active(turn.db).await?.is_empty() {
        let none = turn.templates.no_categories();
        return Ok(vec![none, menu::main_menu(&turn.actor)]);
    }
    let draft = ProductFlow::new(Vec::new());
    let ask = prompt(turn, config, &draft, Step::Photos).await?;
    let session = Session {
        step: Step::Photos,
        draft,
    };
    flow::start(turn, session, ask).await
}

/// A photo that no flow took, sent in `message_id` by a seller (FR-UI-14): an offer to list it,
/// as the first photo of a new product. Until the seller says what it is, nothing is listed.
pub async fn photo_sent(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    message_id: &str,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    if categories::active(turn.db).await?.is_empty() {
        let none = turn.templates.no_categories();
        return Ok(vec![none, menu::main_menu(&turn.actor)]);
    }
    let id = match receive(turn, config, line, message_id).await? {
        Ok(id) => id,
        Err(rejected) => {
            let rejected = Message::text(turn.templates.image_rejected(rejected));
            return Ok(vec![rejected, menu::main_menu(&turn.actor)]);
        }
    };
    let draft = ProductFlow::new(vec![id]);
    let left = max_photos(config).saturating_sub(draft.photos.len());
    let offer = turn.templates.listing_offered(left);
    // With room for no more, on to the name
    let (step, ask) = if left == 0 {
        let step = after(&draft, Step::Photos);
        let mut ask = vec![offer];
        ask.extend(prompt(turn, config, &draft, step).await?);
        (step, ask)
    } else {
        let choices = choices(turn, config, &draft, Step::Photos, 0).await?;
        (Step::Photos, with_choices(vec![offer], choices))
    };
    flow::start(turn, Session { step, draft }, ask).await
}

/// The current step asked again.
async fn again(
    turn: &Turn<'_>,
    config: &Config,
    session: Session<ProductFlow>,
) -> Result<Vec<Message>, DbErr> {
    let ask = prompt(turn, config, &session.draft, session.step).await?;
    flow::answered(turn, Answer::<ProductFlow>::Again(ask)).await
}

/// A button of a flow no longer in progress: listed, cancelled or expired.
fn gone(turn: &Turn<'_>) -> Message {
    let add = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    turn.templates.product_flow_gone(add)
}

/// The answer to the step the user is on (FR-UI-12 step 3). A photo is taken at any step;
/// `None` for a location, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    session: Session<ProductFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    // A suspension mid-flow ends it (NFR-SEC-10)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        let done = Answer::<ProductFlow>::Done(vec![turn.templates.denied(denied)]);
        return flow::answered(turn, done).await.map(Some);
    }
    let Session { step, mut draft } = session;
    let text = match input {
        Input::Image { message_id } => {
            return photo(turn, config, line, step, draft, message_id)
                .await
                .map(Some);
        }
        Input::Text(text) => text,
        _ => return Ok(None),
    };
    if step == Step::Summary {
        let ask = prompt(turn, config, &draft, step).await?;
        return flow::answered(turn, Answer::<ProductFlow>::Again(ask))
            .await
            .map(Some);
    }
    let messages = match take(turn, &mut draft, step, text).await? {
        Ok(taken) => {
            draft.revision += 1;
            let next = match taken {
                // Back to the preview, unchanged
                Taken::Kept => draft.missing().unwrap_or(Step::Summary),
                Taken::Changed => after(&draft, step),
                Taken::Again => step,
            };
            if next == Step::Summary {
                draft.editing = true;
            }
            let ask = prompt(turn, config, &draft, next).await?;
            let session = Session { step: next, draft };
            flow::answered(turn, Answer::Next(session, ask)).await?
        }
        Err(refused) => {
            // Why, with the step's choices; or, for nothing at all, the step's question again
            let again = match why(turn, &draft, refused) {
                Some(why) => with_choices(vec![why], choices(turn, config, &draft, step, 0).await?),
                None => prompt(turn, config, &draft, step).await?,
            };
            flow::answered(turn, Answer::<ProductFlow>::Again(again)).await?
        }
    };
    Ok(Some(messages))
}

/// What an answer that was taken did to the part asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Taken {
    /// "ใช้ของเดิม": left as it was
    Kept,
    Changed,
    /// Changed, and asked again: the photos removed, for others to be sent
    Again,
}

/// Why an answer wasn't taken; the step asks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// Nothing but spaces
    Empty,
    /// Longer than the step takes, in characters
    TooLong(usize),
    NotANumber(NumberError),
    /// A price under [`MIN_PRICE`]
    TooCheap,
    /// A price over the most it may be
    TooMuch(Money),
    /// A stock over the most it may be
    TooMany(u32),
    /// Not one of the step's choices
    NotAChoice,
    /// A category no longer offered
    InactiveCategory,
    /// Text where photos were asked for
    NotAPhoto,
}

/// Takes the photo the user sent in `message_id` at `step` as one more of the product's, if it
/// has room for it (FR-PROD-04). At the photos step, the flow moves on once they are all there;
/// at another, it asks that step again.
async fn photo(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    step: Step,
    mut draft: ProductFlow,
    message_id: &str,
) -> Result<Vec<Message>, DbErr> {
    let max = max_photos(config);
    let refused = if draft.photos.len() >= max {
        // Not even downloaded
        Some(turn.templates.product_photos_full(max))
    } else {
        match receive(turn, config, line, message_id).await? {
            Ok(id) => {
                draft.photos.push(id);
                None
            }
            Err(rejected) => Some(Message::text(turn.templates.image_rejected(rejected))),
        }
    };
    if let Some(refused) = refused {
        let mut again = vec![refused];
        again.extend(prompt(turn, config, &draft, step).await?);
        return flow::answered(turn, Answer::<ProductFlow>::Again(again)).await;
    }

    draft.revision += 1;
    let next = if step == Step::Photos && draft.photos.len() >= max {
        after(&draft, step)
    } else {
        step
    };
    if next == Step::Summary {
        draft.editing = true;
    }
    // The photos step says itself how many there are
    let mut ask = Vec::new();
    if next != Step::Photos {
        let added = turn.templates.product_photo_added(draft.photos.len(), max);
        ask.push(added);
    }
    ask.extend(prompt(turn, config, &draft, next).await?);
    flow::answered(turn, Answer::Next(Session { step: next, draft }, ask)).await
}

/// Downloads the photo the user sent in `message_id` (FR-LINE-09) and keeps it as their draft.
async fn receive(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    message_id: &str,
) -> Result<Result<Uuid, Rejected>, DbErr> {
    let processed = match images::receive(line, message_id, &config.image).await {
        Ok(Ok(processed)) => processed,
        Ok(Err(rejected)) => return Ok(Err(rejected)),
        Err(e) => {
            warn!(error = %e, "cannot download a product's photo");
            return Ok(Err(Rejected::Unavailable));
        }
    };
    let owner = Owner::Draft(turn.actor.user.id);
    images::store(turn.db, owner, processed, &config.image, turn.now).await
}

/// Takes `text` as the answer to `step` into `draft` (FR-PROD-03).
async fn take(
    turn: &Turn<'_>,
    draft: &mut ProductFlow,
    step: Step,
    text: &str,
) -> Result<Result<Taken, Refused>, DbErr> {
    let text = text.trim();
    let is = |label: &Label| label.as_str() == text;
    if keeps(draft, step) && is(&labels::KEEP) {
        return Ok(Ok(Taken::Kept));
    }
    let none = is(&labels::SKIP) || is(&labels::REMOVE);
    let taken = match step {
        Step::Photos => return photos_done(turn, draft, text).await,
        Step::Name => {
            let name = one_line(text);
            bounded(&name, NAME_CHARS).map(|name| draft.name = Some(name.to_owned()))
        }
        Step::Price => price(text).map(|price| draft.price = Some(price)),
        Step::Unit => {
            let unit = one_line(text);
            bounded(&unit, UNIT_CHARS).map(|unit| draft.unit = Some(unit.to_owned()))
        }
        Step::Category => {
            let found = category::Entity::find()
                .filter(category::Column::Name.eq(text))
                .one(turn.db)
                .await?;
            match found {
                Some(category) if category.is_active => {
                    draft.category_id = Some(category.id);
                    Ok(())
                }
                Some(_) => Err(Refused::InactiveCategory),
                None if text.is_empty() => Err(Refused::Empty),
                None => Err(Refused::NotAChoice),
            }
        }
        Step::Stock => stock(text, draft.unit.as_deref()).map(|stock| draft.stock = Some(stock)),
        Step::Description if none => {
            draft.description = None;
            Ok(())
        }
        Step::Description => bounded(text, DESCRIPTION_CHARS)
            .map(|description| draft.description = Some(description.to_owned())),
        Step::Summary => Err(Refused::NotAChoice),
    };
    Ok(taken.map(|()| Taken::Changed))
}

/// Takes `text`, where photos are asked for, into `draft`: "ถัดไป" or "ข้าม" moves on, and
/// "ลบออก", while correcting, removes the photos for others to be sent.
async fn photos_done(
    turn: &Turn<'_>,
    draft: &mut ProductFlow,
    text: &str,
) -> Result<Result<Taken, Refused>, DbErr> {
    let is = |label: &Label| label.as_str() == text;
    if draft.editing && is(&labels::REMOVE) {
        for id in std::mem::take(&mut draft.photos) {
            images::delete_draft(turn.db, id, turn.actor.user.id).await?;
        }
        return Ok(Ok(Taken::Again));
    }
    Ok(if is(&labels::NEXT) || is(&labels::SKIP) {
        Ok(Taken::Changed)
    } else {
        Err(Refused::NotAPhoto)
    })
}

/// `text` on one line, its runs of spaces and line breaks one space each.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text`, if it is neither empty nor longer than `max` characters.
fn bounded(text: &str, max: usize) -> Result<&str, Refused> {
    match text.chars().count() {
        0 => Err(Refused::Empty),
        chars if chars > max => Err(Refused::TooLong(max)),
        _ => Ok(text),
    }
}

/// `text` as a price: ฿0.01 – ฿100,000, with at most 2 decimals.
fn price(text: &str) -> Result<Money, Refused> {
    if text.is_empty() {
        return Err(Refused::Empty);
    }
    match thai::parse_money(text) {
        Ok(price) if price < MIN_PRICE => Err(Refused::TooCheap),
        Ok(price) if price > MAX_PRICE => Err(Refused::TooMuch(MAX_PRICE)),
        Ok(price) => Ok(price),
        Err(NumberError::TooLarge) => Err(Refused::TooMuch(MAX_PRICE)),
        Err(e) => Err(Refused::NotANumber(e)),
    }
}

/// `text` as the stock: "ไม่จำกัด / ทำตามสั่ง", either half of it alone, or a whole number of
/// at most [`MAX_STOCK`], which may be followed by the product's `unit` (`30 กล่อง`).
fn stock(text: &str, unit: Option<&str>) -> Result<Stock, Refused> {
    if [
        &labels::UNLIMITED_STOCK,
        &labels::UNLIMITED,
        &labels::MADE_TO_ORDER,
    ]
    .iter()
    .any(|label| label.as_str() == text)
    {
        return Ok(Stock::Unlimited);
    }
    let count = unit
        .and_then(|unit| text.strip_suffix(unit))
        .unwrap_or(text)
        .trim_end();
    if count.is_empty() {
        return Err(Refused::Empty);
    }
    match thai::parse_whole(count) {
        Ok(count) if count > MAX_STOCK => Err(Refused::TooMany(MAX_STOCK)),
        Ok(count) => Ok(Stock::Count(i32::try_from(count).expect("at most 99,999"))),
        Err(NumberError::TooLarge) => Err(Refused::TooMany(MAX_STOCK)),
        Err(e) => Err(Refused::NotANumber(e)),
    }
}

/// Whether `step` offers "ใช้ของเดิม": while correcting a part that has a value. The photos
/// step has "ถัดไป" instead.
fn keeps(draft: &ProductFlow, step: Step) -> bool {
    draft.editing && draft.has(step) && !matches!(step, Step::Photos | Step::Summary)
}

/// What goes without the description: "ลบออก" for one given while correcting, "ข้าม" otherwise.
fn none_label(draft: &ProductFlow) -> &'static Label {
    if draft.editing && draft.description.is_some() {
        &labels::REMOVE
    } else {
        &labels::SKIP
    }
}

/// What moves on from the photos step: "ถัดไป" once there are some, "ข้าม" otherwise.
fn photos_done_label(draft: &ProductFlow) -> &'static Label {
    if draft.photos.is_empty() {
        &labels::SKIP
    } else {
        &labels::NEXT
    }
}

/// The buttons that correct each part.
fn parts(draft: &ProductFlow) -> Vec<Action> {
    Step::PARTS
        .into_iter()
        .map(|part| {
            let postback = Postback::new(PART)
                .with_name(WHICH, part.key())
                .with_int(REVISION, draft.revision);
            postback.button(part.label().clone())
        })
        .collect()
}

/// Why `step` didn't take an answer, before its choices; `None` when it is enough to ask again.
fn why(turn: &Turn<'_>, draft: &ProductFlow, refused: Refused) -> Option<Message> {
    let templates = turn.templates;
    Some(match refused {
        Refused::TooLong(max) => templates.text_too_long(max),
        Refused::NotANumber(e) => Message::text(templates.number_problem(e)),
        Refused::TooCheap => templates.price_too_low(MIN_PRICE),
        Refused::TooMuch(max) => templates.amount_too_large(max),
        Refused::TooMany(max) => templates.stock_too_large(max),
        Refused::InactiveCategory => templates.category_inactive(),
        Refused::NotAPhoto => templates.photo_expected(photos_done_label(draft)),
        Refused::Empty | Refused::NotAChoice => return None,
    })
}

/// What asks for `step` of `draft`.
async fn prompt(
    turn: &Turn<'_>,
    config: &Config,
    draft: &ProductFlow,
    step: Step,
) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    // What was given before is shown only to correct it
    let current = |value: Option<String>| value.filter(|_| draft.editing);
    let unit = draft.unit.as_deref().unwrap_or_default();
    let ask = match step {
        Step::Photos => {
            templates.ask_product_photos(draft.photos.len(), max_photos(config), draft.editing)
        }
        Step::Name => {
            templates.ask_product_name(NAME_CHARS, current(draft.name.clone()).as_deref())
        }
        Step::Price => templates.ask_product_price(
            MAX_PRICE,
            current(draft.price.map(|price| price.to_string())).as_deref(),
        ),
        Step::Unit => {
            templates.ask_product_unit(UNIT_CHARS, current(draft.unit.clone()).as_deref())
        }
        Step::Category => return ask_category(turn, config, draft, 0).await,
        Step::Stock => templates.ask_product_stock(
            MAX_STOCK,
            unit,
            current(
                draft
                    .stock
                    .map(|stock| templates::stock_line(stock.column(), unit)),
            )
            .as_deref(),
        ),
        Step::Description => templates.ask_product_description(
            DESCRIPTION_CHARS,
            none_label(draft),
            current(draft.description.clone()).as_deref(),
        ),
        Step::Summary => return summary(turn, config, draft).await,
    };
    Ok(with_choices(
        vec![ask],
        choices(turn, config, draft, step, 0).await?,
    ))
}

/// Asks for the category, with page `page` of the active ones as its choices. Without any, it
/// says so; the seller can only cancel then.
async fn ask_category(
    turn: &Turn<'_>,
    config: &Config,
    draft: &ProductFlow,
    page: usize,
) -> Result<Vec<Message>, DbErr> {
    let choices = choices(turn, config, draft, Step::Category, page).await?;
    // Nothing but "ใช้ของเดิม", if even that, without an active category
    let offered = choices.len() > usize::from(keeps(draft, Step::Category));
    let ask = if !offered {
        turn.templates.no_categories()
    } else {
        let current = match draft.category_id.filter(|_| draft.editing) {
            Some(id) => category::Entity::find_by_id(id)
                .one(turn.db)
                .await?
                .map(|category| category.name),
            None => None,
        };
        turn.templates.ask_product_category(current.as_deref())
    };
    Ok(with_choices(vec![ask], choices))
}

/// The Quick Replies of `step` at page `page` of its choices, besides the cancel button the flow
/// adds (FR-UI-04): at most 12.
async fn choices(
    turn: &Turn<'_>,
    config: &Config,
    draft: &ProductFlow,
    step: Step,
    page: usize,
) -> Result<Vec<Action>, DbErr> {
    let message = |label: &Label| Action::message(label.clone(), label.as_str());
    let keep = keeps(draft, step);
    let mut choices = match step {
        Step::Photos => {
            let mut choices = Vec::new();
            if draft.photos.len() < max_photos(config) {
                choices.push(Action::Camera {
                    label: labels::TAKE_PHOTO.clone(),
                });
                choices.push(Action::CameraRoll {
                    label: labels::CHOOSE_PHOTO.clone(),
                });
            }
            choices.push(message(photos_done_label(draft)));
            if draft.editing && !draft.photos.is_empty() {
                choices.push(message(&labels::REMOVE));
            }
            choices
        }
        Step::Name | Step::Price | Step::Summary => Vec::new(),
        Step::Unit => {
            let room = limits::QUICK_REPLY_ITEMS - 1 - usize::from(keep);
            labels::UNITS.iter().take(room).map(message).collect()
        }
        Step::Category => category_choices(turn, draft, page).await?,
        Step::Stock => vec![message(&labels::UNLIMITED_STOCK)],
        Step::Description => vec![message(none_label(draft))],
    };
    if keep {
        choices.push(message(&labels::KEEP));
    }
    Ok(choices)
}

/// Page `page` of the active categories, in the order admins set, as Quick Replies that send
/// their names, with "ดูเพิ่มเติม" while more remain. A page past the last, from a button sent
/// before categories were deactivated, starts over.
async fn category_choices(
    turn: &Turn<'_>,
    draft: &ProductFlow,
    page: usize,
) -> Result<Vec<Action>, DbErr> {
    let active = categories::active(turn.db).await?;
    // Room for "ดูเพิ่มเติม", the cancel button and, while correcting, "ใช้ของเดิม"
    let per_page = limits::QUICK_REPLY_ITEMS - 2 - usize::from(keeps(draft, Step::Category));
    let pages = active.len().div_ceil(per_page);
    let page = if page < pages { page } else { 0 };
    let choice = |category: &category::Model| {
        Action::message(Label::fit(category.name.as_str()), category.name.as_str())
    };
    let more = |next: usize| {
        let next = i64::try_from(next).expect("a few pages");
        Postback::new(CATEGORIES).with_int(PAGE, next).to_string()
    };
    Ok(templates::quick_reply_page(
        &active, page, per_page, choice, more,
    ))
}

/// The preview of the product (FR-PROD-01), with the buttons that list it or correct a part.
async fn summary(
    turn: &Turn<'_>,
    config: &Config,
    draft: &ProductFlow,
) -> Result<Vec<Message>, DbErr> {
    let category = match draft.category_id {
        Some(id) => category::Entity::find_by_id(id).one(turn.db).await?,
        None => None,
    };
    let button = |action, label: &Label| {
        let postback = Postback::new(action).with_int(REVISION, draft.revision);
        postback.button(label.clone())
    };
    let photo = draft
        .photos
        .first()
        .map(|&id| images::product_image_url(&config.app.base_url, id));
    let preview = ProductPreview {
        name: draft.name.as_deref().unwrap_or_default(),
        price: draft.price.unwrap_or(Money::ZERO),
        unit: draft.unit.as_deref().unwrap_or_default(),
        category: category.as_ref().map_or("", |category| &category.name),
        stock: draft.stock.and_then(Stock::column),
        description: draft.description.as_deref(),
        photo: photo.as_ref(),
        photos: draft.photos.len(),
        buttons: vec![
            button(SUBMIT, &labels::CONFIRM_LISTING),
            button(EDIT, &labels::EDIT),
        ],
    };
    Ok(turn.templates.product_preview(&preview))
}

/// Lists the product (FR-PROD-01) in `shop`, the seller's: `ACTIVE`, so buyers see it from now
/// on, with its photos in the order sent. Its category must still be active, and its photos
/// still there.
async fn submit(
    turn: &Turn<'_>,
    config: &Config,
    shop: &shop::Model,
    mut draft: ProductFlow,
) -> Result<Vec<Message>, DbErr> {
    let (Some(name), Some(price), Some(unit), Some(category_id), Some(stock)) = (
        draft.name.clone(),
        draft.price,
        draft.unit.clone(),
        draft.category_id,
        draft.stock,
    ) else {
        let step = draft.missing().expect("a part is missing");
        let ask = prompt(turn, config, &draft, step).await?;
        return flow::answered(turn, Answer::Next(Session { step, draft }, ask)).await;
    };
    let user_id = turn.actor.user.id;
    // The product and its photos are saved together, or not at all
    let txn = turn.db.begin().await?;
    // Locked, so it can't be deleted until the product is in it
    let category = category::Entity::find_by_id(category_id)
        .lock_shared()
        .one(&txn)
        .await?;
    if !category.is_some_and(|category| category.is_active) {
        txn.rollback().await?;
        draft.category_id = None;
        draft.revision += 1;
        let mut ask = vec![turn.templates.category_inactive()];
        ask.extend(prompt(turn, config, &draft, Step::Category).await?);
        let session = Session {
            step: Step::Category,
            draft,
        };
        return flow::answered(turn, Answer::Next(session, ask)).await;
    }
    let listed = product::ActiveModel {
        shop_id: Set(shop.id),
        category_id: Set(category_id),
        name: Set(name),
        description: Set(draft.description.clone()),
        price: Set(price),
        unit: Set(unit),
        stock: Set(stock.column()),
        status: Set(product::Status::Active),
        created_at: Set(turn.now),
        updated_at: Set(turn.now),
        deleted_at: Set(None),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    if !images::adopt_all(&txn, &draft.photos, user_id, Owner::Product(listed.id)).await? {
        // Nothing is listed; the photos left are sent again with the others
        txn.rollback().await?;
        for id in std::mem::take(&mut draft.photos) {
            images::delete_draft(turn.db, id, user_id).await?;
        }
        draft.revision += 1;
        let mut ask = vec![turn.templates.product_photos_lost()];
        ask.extend(prompt(turn, config, &draft, Step::Photos).await?);
        let session = Session {
            step: Step::Photos,
            draft,
        };
        return flow::answered(turn, Answer::Next(session, ask)).await;
    }
    txn.commit().await?;

    // #27 moderates the name, description and photos here, and may hold the product (FR-PROD-09)
    info!(
        product_id = listed.id,
        shop_id = shop.id,
        photos = draft.photos.len(),
        "product listed"
    );
    let item = |item: Item, label: &Label| item.postback().button(label.clone());
    let next = vec![
        item(Item::MyProducts, &labels::MY_PRODUCTS),
        item(Item::AddProduct, &labels::ADD_ANOTHER),
    ];
    let done = turn.templates.product_listed(&listed.name, next);
    flow::answered(turn, Answer::<ProductFlow>::Done(vec![done])).await
}

/// Whether a product whose status is `from` may become `to` (FR-PROD-07). A product is listed
/// from `DRAFT`; its seller hides and shows it (#26); a new or edited one may be held for review
/// (#27), and an admin then lists or removes it (#31); any but a deleted one may be deleted, for
/// good (DR-04). Sold out isn't a status: it is the stock (FR-PROD-06).
pub fn may_become(from: product::Status, to: product::Status) -> bool {
    use product::Status::{Active, Deleted, Draft, Held, Hidden, Removed};
    match (from, to) {
        (Deleted, _) => false,
        (_, Deleted) => true,
        (Draft, Active | Held) | (Active, Hidden | Held) | (Hidden, Active | Held) => true,
        (Held, Active | Removed) => true,
        _ => false,
    }
}

/// Whether a buyer can order a product now, as its card says (FR-SRCH-07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// *เหลือ 5*: this many left for new orders (FR-PROD-05)
    Left(i32),
    /// *สั่งได้*: unlimited or made to order
    Orderable,
    /// *หมด* (FR-PROD-06)
    SoldOut,
    /// *ร้านปิด*: its shop takes no orders now (FR-SHOP-05)
    ShopClosed,
}

impl Availability {
    /// That of `product` of `shop`. Whether the product is for sale at all, `ACTIVE`, is the
    /// caller's check: lists show only those (FR-SRCH-02), and the cart flags the others
    /// (FR-CART-03). A shop that doesn't take orders makes it [`ShopClosed`](Self::ShopClosed)
    /// whatever its stock; one not `APPROVED` has its products hidden anyway (FR-SHOP-10).
    pub fn of(product: &product::Model, shop: &shop::Model) -> Self {
        if !shops::takes_orders(shop) {
            return Self::ShopClosed;
        }
        match product.stock {
            None => Self::Orderable,
            Some(0) => Self::SoldOut,
            Some(left) => Self::Left(left),
        }
    }

    /// Whether it can be added to a cart or checked out.
    pub fn orderable(self) -> bool {
        matches!(self, Self::Left(_) | Self::Orderable)
    }
}
