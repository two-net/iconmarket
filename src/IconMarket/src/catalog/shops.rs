//! Shops (FR-SHOP): applying to open one ("เปิดร้าน"), and "ร้านของฉัน". Built by #23 and #24;
//! running one, "เปิด/ปิดร้าน" and "ตั้งค่าร้าน", is [`settings`].
//!
//! A shop's status (FR-SHOP-10) is `PENDING` from its application until an admin approves it
//! or turns it down ("อนุมัติร้าน", [`approvals`]), or `APPROVED` at once while
//! `shop.requireApproval` is off (FR-SHOP-03). Turned down, it is `REJECTED`, and its owner may
//! apply again, which makes the same shop `PENDING` once more: a user owns at most one shop
//! (FR-SHOP-02). Only an `APPROVED` shop sells: its owner manages it ([`authorize`]), buyers see
//! its products (#28) and, while it is open, it [takes orders](takes_orders) (#32). One whose
//! edited name, description or photo [moderation] finds suspicious is `HELD` for review: its
//! owner still runs it, but buyers don't see it until an admin approves it again (#31), and it
//! takes no new orders. A `SUSPENDED` one (#31) is hidden and takes no new orders, though its
//! open orders go on; `CLOSED` is for good (#36).
//!
//! An application is moderated too: one found suspicious is left `PENDING` for an admin to
//! approve, even while `shop.requireApproval` is off.
//!
//! The application is a [flow](ShopFlow), which asks for the address first if the user has
//! none (FR-ONB-04). Its last step is a summary, whose buttons send the application or correct
//! one part; they carry the draft's revision, so only the latest summary acts (FR-UI-13).
//! Nothing is saved until the application is sent but its photo, which is downloaded as it
//! arrives (FR-LINE-09) and kept as a [draft](Owner::Draft) of the user's until then. What the
//! user types is never logged (NFR-PRIV-05).

pub mod settings;
#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};
use url::Url;

use super::images::{self, Owner, Rejected};
use super::moderation::{self, Held, Subject};
use super::{browse, products};
use crate::admin::approvals;
use crate::admin::audit::{self, Entry, Target};
use crate::ai;
use crate::config::{Config, PaymentMethod};
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::{self, Item};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Location, Turn};
use crate::line::{self, Action, Message};
use crate::notifications::templates::{
    self, Applicant, PromptPay, Running, ShopCard, ShopFront, labels, with_choices,
};
use crate::notifications::{Kind, To};
use crate::payments;
use crate::persistence::entity::{shop, user};
use crate::thai::{self, Label, Money, NumberError};
use crate::users::access::{self, Actor, Denied, authorize};
use crate::users::address::{self, Ask, Point, Then};

/// The actions of the summary's buttons, which carry the draft's revision in `r`: send the
/// application, or ask which part to correct
pub const SUBMIT: &str = "open_shop.submit";
pub const EDIT: &str = "open_shop.edit";
/// Correct the part named in `p`
pub const PART: &str = "open_shop.part";
/// Apply again, once turned down (FR-SHOP-04)
pub const AGAIN: &str = "open_shop.again";
const REVISION: &str = "r";
const WHICH: &str = "p";

/// FR-SHOP-01
pub const NAME_CHARS: usize = 50;
pub const DESCRIPTION_CHARS: usize = 300;
/// Room for the user's own address as one line, which "ใช้ที่อยู่บ้าน" takes
pub const PICKUP_ADDRESS_CHARS: usize = 200;
pub const MAX_DELIVERY_FEE: Money = Money::from_baht(1_000);
/// As much as a product may cost (FR-PROD-03)
pub const MAX_MINIMUM_ORDER: Money = products::MAX_PRICE;
/// Room for *ทุกวัน 06:00–10:00, ปิดวันจันทร์* and the like (FR-SHOP-07)
pub const OPENING_HOURS_CHARS: usize = 50;

/// The application being filled in (FR-SHOP-01). A part left `None` once its step is past
/// means "none": no photo, no minimum order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShopFlow {
    name: Option<String>,
    description: Option<String>,
    photo: Option<Photo>,
    pickup_address: Option<String>,
    pickup_location: Option<Point>,
    phone: Option<String>,
    fulfillment: Option<Fulfillment>,
    delivery_fee: Money,
    minimum_order: Option<Money>,
    promptpay_id: Option<String>,
    /// Set only in "ตั้งค่าร้าน"; missing from drafts saved before it was added
    #[serde(default)]
    opening_hours: Option<String>,
    /// Whether every part has been given, so that each answer leads back to the summary
    editing: bool,
    /// How many answers were taken, so that a summary's buttons act only on what it showed
    revision: i64,
}

/// The shop's photo, as the application has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Photo {
    /// Sent in this flow: a draft image of the user's
    New(Uuid),
    /// The turned-down shop's, kept unless changed
    Current(Uuid),
}

impl Photo {
    fn id(self) -> Uuid {
        match self {
            Self::New(id) | Self::Current(id) => id,
        }
    }
}

/// How buyers get their orders (FR-SHOP-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fulfillment {
    Pickup,
    Delivery,
    Both,
}

impl Fulfillment {
    const ALL: [(Self, &'static Label); 3] = [
        (Self::Pickup, &labels::PICKUP_ONLY),
        (Self::Delivery, &labels::DELIVERY_ONLY),
        (Self::Both, &labels::PICKUP_AND_DELIVERY),
    ];

    fn of(pickup: bool, delivery: bool) -> Option<Self> {
        match (pickup, delivery) {
            (true, false) => Some(Self::Pickup),
            (false, true) => Some(Self::Delivery),
            (true, true) => Some(Self::Both),
            (false, false) => None,
        }
    }

    fn pickup(self) -> bool {
        self != Self::Delivery
    }

    fn delivery(self) -> bool {
        self != Self::Pickup
    }

    fn label(self) -> &'static Label {
        Self::ALL
            .iter()
            .find(|(fulfillment, _)| *fulfillment == self)
            .map(|&(_, label)| label)
            .expect("every choice is listed")
    }
}

/// The steps, in the order a first application asks them. Each but the summary and the opening
/// hours is also a part the summary can correct; all but the summary are parts of
/// "ตั้งค่าร้าน" ([`settings`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Name,
    Description,
    Photo,
    PickupAddress,
    PickupLocation,
    Phone,
    Fulfillment,
    /// Only when the shop delivers
    DeliveryFee,
    MinimumOrder,
    /// Only while `payment.methods` has PromptPay
    PromptPay,
    /// Only in "ตั้งค่าร้าน" (FR-SHOP-07)
    OpeningHours,
    Summary,
}

impl Flow for ShopFlow {
    const NAME: &'static str = "shop_application";
    type Step = Step;
}

impl Step {
    /// The parts, in the order the summary offers them.
    const PARTS: [Self; 10] = [
        Self::Name,
        Self::Description,
        Self::Photo,
        Self::PickupAddress,
        Self::PickupLocation,
        Self::Phone,
        Self::Fulfillment,
        Self::DeliveryFee,
        Self::MinimumOrder,
        Self::PromptPay,
    ];

    /// Its name in the postback correcting it.
    fn key(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Description => "description",
            Self::Photo => "photo",
            Self::PickupAddress => "pickup_address",
            Self::PickupLocation => "pickup_location",
            Self::Phone => "phone",
            Self::Fulfillment => "fulfillment",
            Self::DeliveryFee => "delivery_fee",
            Self::MinimumOrder => "minimum_order",
            Self::PromptPay => "promptpay",
            Self::OpeningHours => "opening_hours",
            Self::Summary => "summary",
        }
    }

    fn label(self) -> &'static Label {
        match self {
            Self::Name => &labels::SHOP_NAME,
            Self::Description => &labels::SHOP_DESCRIPTION,
            Self::Photo => &labels::SHOP_PHOTO,
            Self::PickupAddress => &labels::PICKUP_ADDRESS,
            Self::PickupLocation => &labels::PICKUP_LOCATION,
            Self::Phone => &labels::SHOP_PHONE,
            Self::Fulfillment => &labels::FULFILLMENT,
            Self::DeliveryFee => &labels::DELIVERY_FEE,
            Self::MinimumOrder => &labels::MINIMUM_ORDER,
            Self::PromptPay => &labels::PROMPTPAY,
            Self::OpeningHours => &labels::OPENING_HOURS,
            Self::Summary => &labels::EDIT,
        }
    }

    fn part(key: &str) -> Option<Self> {
        Self::PARTS.into_iter().find(|step| step.key() == key)
    }
}

impl ShopFlow {
    /// A first application, with nothing given yet.
    fn new() -> Self {
        Self {
            name: None,
            description: None,
            photo: None,
            pickup_address: None,
            pickup_location: None,
            phone: None,
            fulfillment: None,
            delivery_fee: Money::ZERO,
            minimum_order: None,
            promptpay_id: None,
            opening_hours: None,
            editing: false,
            revision: 0,
        }
    }

    /// What `shop` has, its photo `photo`, to correct: applying again once turned down, or in
    /// "ตั้งค่าร้าน".
    fn from_shop(shop: &shop::Model, photo: Option<Uuid>) -> Self {
        let pickup_location =
            shop.pickup_latitude
                .zip(shop.pickup_longitude)
                .map(|(latitude, longitude)| Point {
                    latitude,
                    longitude,
                });
        Self {
            name: Some(shop.name.clone()),
            description: Some(shop.description.clone()),
            photo: photo.map(Photo::Current),
            pickup_address: shop.pickup_address.clone(),
            pickup_location,
            phone: shop.phone.clone(),
            fulfillment: Fulfillment::of(shop.offers_pickup, shop.offers_delivery),
            delivery_fee: shop.delivery_fee,
            minimum_order: shop.minimum_order,
            promptpay_id: shop.promptpay_id.clone(),
            opening_hours: shop.opening_hours.clone(),
            editing: true,
            revision: 0,
        }
    }

    fn delivers(&self) -> bool {
        self.fulfillment.is_some_and(Fulfillment::delivery)
    }

    /// The first part the shop can't do without, if one is missing: its name, description,
    /// phone, how buyers get their orders, and an address or a location.
    fn missing(&self) -> Option<Step> {
        if self.name.is_none() {
            Some(Step::Name)
        } else if self.description.is_none() {
            Some(Step::Description)
        } else if self.pickup_address.is_none() && self.pickup_location.is_none() {
            Some(Step::PickupAddress)
        } else if self.phone.is_none() {
            Some(Step::Phone)
        } else if self.fulfillment.is_none() {
            Some(Step::Fulfillment)
        } else {
            None
        }
    }

    /// Whether `step` has a value to keep while the application is being corrected.
    fn has(&self, step: Step) -> bool {
        match step {
            Step::Name => self.name.is_some(),
            Step::Description => self.description.is_some(),
            Step::Photo => self.photo.is_some(),
            Step::PickupAddress => self.pickup_address.is_some(),
            Step::PickupLocation => self.pickup_location.is_some(),
            Step::Phone => self.phone.is_some(),
            Step::Fulfillment => self.fulfillment.is_some(),
            Step::DeliveryFee => true,
            Step::MinimumOrder => self.minimum_order.is_some(),
            Step::PromptPay => self.promptpay_id.is_some(),
            Step::OpeningHours => self.opening_hours.is_some(),
            Step::Summary => false,
        }
    }
}

/// Whether the application asks for a PromptPay ID: only while buyers can pay by PromptPay.
fn asks_promptpay(config: &Config) -> bool {
    config.payment.methods.contains(&PaymentMethod::PromptPay)
}

/// The step after `step` was answered: the next one a first application asks, or, while
/// correcting, back to the summary; or the first part still missing.
fn after(config: &Config, draft: &ShopFlow, step: Step) -> Step {
    let next = if draft.editing {
        match step {
            Step::Fulfillment if draft.delivers() => Step::DeliveryFee,
            _ => Step::Summary,
        }
    } else {
        match step {
            Step::Name => Step::Description,
            Step::Description => Step::Photo,
            Step::Photo => Step::PickupAddress,
            Step::PickupAddress => Step::PickupLocation,
            Step::PickupLocation => Step::Phone,
            Step::Phone => Step::Fulfillment,
            Step::Fulfillment if draft.delivers() => Step::DeliveryFee,
            Step::Fulfillment | Step::DeliveryFee => Step::MinimumOrder,
            Step::MinimumOrder if asks_promptpay(config) => Step::PromptPay,
            Step::MinimumOrder | Step::PromptPay | Step::OpeningHours | Step::Summary => {
                Step::Summary
            }
        }
    };
    match next {
        Step::Summary => draft.missing().unwrap_or(Step::Summary),
        next => next,
    }
}

/// A tap on "เปิดร้าน" or "ร้านของฉัน", or on one of the application's buttons. A button of a
/// summary that is not the latest shows the application as it is now; one of an application
/// no longer in progress shows how the user's shop stands, and never starts another flow.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ApplyForShop).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let session = turn.flow.as_ref().and_then(|flow| flow.read::<ShopFlow>());
    let action = postback.action();
    if action == Item::MyShop.action() {
        return match session {
            Some(session) => again(turn, config, session).await,
            None => open(turn, config, &turn.actor.user).await,
        };
    }
    if action == AGAIN {
        return match session {
            Some(session) => again(turn, config, session).await,
            None => reapply(turn, config).await,
        };
    }
    let Some(session) = session else {
        return stale(turn, config).await;
    };
    let latest =
        session.step == Step::Summary && postback.int(REVISION) == Some(session.draft.revision);
    if !latest {
        return again(turn, config, session).await;
    }
    match (action, postback.name(WHICH).and_then(Step::part)) {
        (SUBMIT, _) => submit(turn, config, ai, session.draft).await,
        (EDIT, _) => {
            let parts = parts(config, &session.draft);
            let choose = turn.templates.choose_part(parts);
            flow::answered(turn, Answer::<ShopFlow>::Again(vec![choose])).await
        }
        (PART, Some(part)) if offered(config, &session.draft, part) => {
            let ask = prompt(turn, config, &turn.actor.user, &session.draft, part);
            let session = Session {
                step: part,
                draft: session.draft,
            };
            flow::answered(turn, Answer::Next(session, ask)).await
        }
        _ => again(turn, config, session).await,
    }
}

/// "เปิดร้าน" or "ร้านของฉัน" for `user`, as they are now: the application if they have no
/// shop, after the address if they have none (FR-ONB-04); otherwise how their shop stands.
pub async fn open(
    turn: &Turn<'_>,
    config: &Config,
    user: &user::Model,
) -> Result<Vec<Message>, DbErr> {
    let Some(shop) = &turn.actor.shop else {
        if !address::has_address(user) {
            return address::start(turn, config, Ask::All, Then::OpenShop).await;
        }
        let draft = ShopFlow::new();
        let ask = prompt(turn, config, user, &draft, Step::Name);
        let session = Session {
            step: Step::Name,
            draft,
        };
        return flow::start(turn, session, ask).await;
    };
    standing(turn, config, shop).await
}

/// "สมัครใหม่" (FR-SHOP-04): the turned-down application, to correct and send again.
async fn reapply(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    let Some(shop) = turn
        .actor
        .shop
        .as_ref()
        .filter(|shop| shop.status == shop::Status::Rejected)
    else {
        return stale(turn, config).await;
    };
    if !address::has_address(&turn.actor.user) {
        return address::start(turn, config, Ask::All, Then::OpenShop).await;
    }
    let photo = images::first(turn.db, Owner::Shop(shop.id)).await?;
    let draft = ShopFlow::from_shop(shop, photo);
    let step = draft.missing().unwrap_or(Step::Summary);
    let ask = prompt(turn, config, &turn.actor.user, &draft, step);
    flow::start(turn, Session { step, draft }, ask).await
}

/// The current step asked again.
async fn again(
    turn: &Turn<'_>,
    config: &Config,
    session: Session<ShopFlow>,
) -> Result<Vec<Message>, DbErr> {
    let ask = prompt(turn, config, &turn.actor.user, &session.draft, session.step);
    flow::answered(turn, Answer::<ShopFlow>::Again(ask)).await
}

/// A button of an application no longer in progress: how the user's shop stands, or, without
/// one, a way to start again.
async fn stale(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    match &turn.actor.shop {
        Some(shop) => standing(turn, config, shop).await,
        None => {
            let open = Item::MyShop.postback().button(labels::OPEN_SHOP.clone());
            Ok(vec![turn.templates.application_gone(open)])
        }
    }
}

/// How `shop`, the user's, stands. Waiting for an admin, it can't be applied for again
/// (FR-SHOP-02).
async fn standing(
    turn: &Turn<'_>,
    config: &Config,
    shop: &shop::Model,
) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let photo = photo_url(config, images::first(turn.db, Owner::Shop(shop.id)).await?);
    let owners = || templates.shop(&card(shop, photo.as_ref(), None, Vec::new()));
    Ok(match shop.status {
        shop::Status::Pending => vec![
            owners(),
            templates.application_pending(&shop.name, shop.updated_at),
        ],
        shop::Status::Rejected => {
            let again = Postback::new(AGAIN).button(labels::APPLY_AGAIN.clone());
            let reason = shop.rejection_reason.as_deref();
            vec![templates.application_rejected(&shop.name, reason, again)]
        }
        shop::Status::Approved | shop::Status::Held => {
            let item = |item: Item, label: &Label| item.postback().button(label.clone());
            let actions = vec![
                settings::switch(shop.is_open),
                item(Item::AddProduct, &labels::ADD_PRODUCT),
                item(Item::MyProducts, &labels::MY_PRODUCTS),
                item(Item::ShopSettings, &labels::SHOP_SETTINGS),
                settings::front_button(),
            ];
            let mine = if shop.status == shop::Status::Held {
                templates.my_shop_held(&shop.name, actions)
            } else {
                templates.my_shop(&shop.name, shop.is_open, actions)
            };
            vec![owners(), mine]
        }
        shop::Status::Suspended => vec![templates.denied(Denied::ShopSuspended)],
        shop::Status::Closed => vec![templates.denied(Denied::NotAllowed)],
    })
}

/// The answer to the step the user is on (FR-UI-12 step 3). `None` when the step doesn't take
/// this kind of input, such as a photo where a name is asked for, so routing goes on
/// (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    session: Session<ShopFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    // A suspension mid-flow ends it (NFR-SEC-10)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ApplyForShop).await? {
        let done = Answer::<ShopFlow>::Done(vec![turn.templates.denied(denied)]);
        return flow::answered(turn, done).await.map(Some);
    }
    let Session { step, mut draft } = session;
    if let (Step::Summary, Input::Text(_)) = (step, input) {
        let ask = prompt(turn, config, &turn.actor.user, &draft, step);
        return flow::answered(turn, Answer::<ShopFlow>::Again(ask))
            .await
            .map(Some);
    }
    let Some(taken) = take_input(turn, config, line, &mut draft, step, input).await? else {
        return Ok(None);
    };
    let user = &turn.actor.user;
    let messages = match taken {
        Ok(taken) => {
            draft.revision += 1;
            let next = match taken {
                // Back to the summary, unchanged
                Taken::Kept => draft.missing().unwrap_or(Step::Summary),
                Taken::Changed => after(config, &draft, step),
            };
            if next == Step::Summary {
                draft.editing = true;
            }
            let ask = prompt(turn, config, user, &draft, next);
            let session = Session { step: next, draft };
            flow::answered(turn, Answer::Next(session, ask)).await?
        }
        Err(refused) => {
            // Why, with the step's choices; or, for nothing at all, the step's question again
            let again = match why(turn, &draft, step, refused) {
                Some(why) => with_choices(vec![why], choices(user, &draft, step)),
                None => prompt(turn, config, user, &draft, step),
            };
            flow::answered(turn, Answer::<ShopFlow>::Again(again)).await?
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
}

/// Takes `input` as the answer to `step` into `draft`, for the application and "ตั้งค่าร้าน"
/// alike: why it wasn't taken, if it wasn't; `None` when the step doesn't take this kind of
/// input, such as a photo where a name is asked for.
async fn take_input(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    draft: &mut ShopFlow,
    step: Step,
    input: &Input,
) -> Result<Option<Result<Taken, Refused>>, DbErr> {
    let taken = match (step, input) {
        (_, Input::Text(text))
            if draft.editing && draft.has(step) && labels::KEEP.as_str() == text.trim() =>
        {
            return Ok(Some(Ok(Taken::Kept)));
        }
        (Step::Photo, Input::Image { message_id }) => {
            photo(turn, config, line, draft, message_id).await?
        }
        // LINE's address text for it isn't needed, so isn't kept (NFR-PRIV-04)
        (
            Step::PickupLocation,
            &Input::Location(Location {
                latitude,
                longitude,
                address: _,
            }),
        ) => {
            draft.pickup_location = Some(Point {
                latitude,
                longitude,
            });
            Ok(())
        }
        (_, Input::Text(text)) => take(turn, draft, step, text).await?,
        _ => return Ok(None),
    };
    Ok(Some(taken.map(|()| Taken::Changed)))
}

/// Why an answer wasn't taken; the step asks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// Nothing but spaces
    Empty,
    /// Longer than the step takes, in characters
    TooLong(usize),
    NotAPhone,
    NotAPromptPayId,
    NotAnAmount(NumberError),
    /// More than the step takes
    TooMuch(Money),
    /// Neither an address nor a location would be left
    NoPickup,
    /// Not one of the step's choices
    NotAChoice,
    /// Text where the photo was asked for
    NotAPhoto,
    /// Text where the location was asked for
    NotALocation,
    /// The photo couldn't be kept
    Photo(Rejected),
}

/// Downloads the photo the user sent in `message_id` (FR-LINE-09) and keeps it as their draft,
/// in place of any they sent before.
async fn photo(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    draft: &mut ShopFlow,
    message_id: &str,
) -> Result<Result<(), Refused>, DbErr> {
    let processed = match images::receive(line, message_id, &config.image).await {
        Ok(Ok(processed)) => processed,
        Ok(Err(rejected)) => return Ok(Err(Refused::Photo(rejected))),
        Err(e) => {
            warn!(error = %e, "cannot download a shop's photo");
            return Ok(Err(Refused::Photo(Rejected::Unavailable)));
        }
    };
    let user_id = turn.actor.user.id;
    let owner = Owner::Draft(user_id);
    let id = match images::store(turn.db, owner, processed, &config.image, turn.now).await? {
        Ok(id) => id,
        Err(rejected) => return Ok(Err(Refused::Photo(rejected))),
    };
    if let Some(Photo::New(earlier)) = draft.photo {
        images::delete_draft(turn.db, earlier, user_id).await?;
    }
    draft.photo = Some(Photo::New(id));
    Ok(Ok(()))
}

/// Takes `text` as the answer to `step` into `draft`.
async fn take(
    turn: &Turn<'_>,
    draft: &mut ShopFlow,
    step: Step,
    text: &str,
) -> Result<Result<(), Refused>, DbErr> {
    let text = text.trim();
    let is = |label: &Label| label.as_str() == text;
    let none = is(&labels::SKIP) || is(&labels::REMOVE);
    let user = &turn.actor.user;
    let taken = match step {
        Step::Name => {
            let name = text.split_whitespace().collect::<Vec<_>>().join(" ");
            bounded(&name, NAME_CHARS).map(|name| draft.name = Some(name.to_owned()))
        }
        Step::Description => bounded(text, DESCRIPTION_CHARS)
            .map(|description| draft.description = Some(description.to_owned())),
        Step::Photo if none => {
            if let Some(Photo::New(id)) = draft.photo.take() {
                images::delete_draft(turn.db, id, user.id).await?;
            }
            Ok(())
        }
        Step::Photo => Err(Refused::NotAPhoto),
        Step::PickupAddress if is(&labels::USE_HOME_ADDRESS) && home(user).is_some() => {
            draft.pickup_address = home(user);
            Ok(())
        }
        // Without an address, a first application asks for the location next, which it then
        // needs; one being corrected must have it already
        Step::PickupAddress if none => {
            if draft.editing && draft.pickup_location.is_none() {
                Err(Refused::NoPickup)
            } else {
                draft.pickup_address = None;
                Ok(())
            }
        }
        Step::PickupAddress => bounded(text, PICKUP_ADDRESS_CHARS)
            .map(|address| draft.pickup_address = Some(address.to_owned())),
        Step::PickupLocation if is(&labels::USE_HOME_LOCATION) && home_point(user).is_some() => {
            draft.pickup_location = home_point(user);
            Ok(())
        }
        Step::PickupLocation if none && draft.pickup_address.is_some() => {
            draft.pickup_location = None;
            Ok(())
        }
        Step::PickupLocation if none => Err(Refused::NoPickup),
        Step::PickupLocation => Err(Refused::NotALocation),
        Step::Phone => match thai::parse_phone(text) {
            Some(phone) => {
                draft.phone = Some(phone);
                Ok(())
            }
            None => Err(Refused::NotAPhone),
        },
        Step::Fulfillment => match Fulfillment::ALL.iter().find(|(_, label)| is(label)) {
            Some(&(fulfillment, _)) => {
                draft.fulfillment = Some(fulfillment);
                if !fulfillment.delivery() {
                    draft.delivery_fee = Money::ZERO;
                }
                Ok(())
            }
            None => Err(Refused::NotAChoice),
        },
        Step::DeliveryFee if is(&labels::NO_DELIVERY_FEE) => {
            draft.delivery_fee = Money::ZERO;
            Ok(())
        }
        Step::DeliveryFee => amount(text, MAX_DELIVERY_FEE).map(|fee| draft.delivery_fee = fee),
        Step::MinimumOrder if none || is(&labels::NO_MINIMUM) => {
            draft.minimum_order = None;
            Ok(())
        }
        Step::MinimumOrder => amount(text, MAX_MINIMUM_ORDER)
            .map(|minimum| draft.minimum_order = Some(minimum).filter(|&m| m != Money::ZERO)),
        Step::PromptPay if none => {
            draft.promptpay_id = None;
            Ok(())
        }
        Step::PromptPay => match payments::parse_promptpay_id(text) {
            Some(id) => {
                draft.promptpay_id = Some(id);
                Ok(())
            }
            None => Err(Refused::NotAPromptPayId),
        },
        Step::OpeningHours if none => {
            draft.opening_hours = None;
            Ok(())
        }
        Step::OpeningHours => bounded(text, OPENING_HOURS_CHARS)
            .map(|hours| draft.opening_hours = Some(hours.to_owned())),
        Step::Summary => Err(Refused::NotAChoice),
    };
    Ok(taken)
}

/// `text`, if it is neither empty nor longer than `max` characters.
fn bounded(text: &str, max: usize) -> Result<&str, Refused> {
    match text.chars().count() {
        0 => Err(Refused::Empty),
        chars if chars > max => Err(Refused::TooLong(max)),
        _ => Ok(text),
    }
}

/// `text` as an amount of at most `max`.
fn amount(text: &str, max: Money) -> Result<Money, Refused> {
    if text.is_empty() {
        return Err(Refused::Empty);
    }
    match thai::parse_money(text) {
        Ok(amount) if amount > max => Err(Refused::TooMuch(max)),
        Ok(amount) => Ok(amount),
        Err(e) => Err(Refused::NotAnAmount(e)),
    }
}

/// `user`'s address as one line, which "ใช้ที่อยู่บ้าน" makes the shop's.
fn home(user: &user::Model) -> Option<String> {
    let (zone, house_number) = (user.zone.as_deref()?, user.house_number.as_deref()?);
    Some(templates::address_line(
        zone,
        house_number,
        user.landmark_note.as_deref(),
    ))
}

/// `user`'s shared location, which "ใช้ตำแหน่งบ้าน" makes the shop's.
fn home_point(user: &user::Model) -> Option<Point> {
    let (latitude, longitude) = user.latitude.zip(user.longitude)?;
    Some(Point {
        latitude,
        longitude,
    })
}

/// What goes without an optional part at `step`: "ลบออก" for one given while correcting,
/// "ข้าม" otherwise; `None` where the part can't be done without.
fn none_label(draft: &ShopFlow, step: Step) -> Option<&'static Label> {
    let allowed = match step {
        Step::Photo | Step::MinimumOrder | Step::PromptPay | Step::OpeningHours => true,
        Step::PickupAddress => !draft.editing || draft.pickup_location.is_some(),
        Step::PickupLocation => draft.pickup_address.is_some(),
        _ => false,
    };
    let label = if draft.editing && draft.has(step) {
        &labels::REMOVE
    } else {
        &labels::SKIP
    };
    allowed.then_some(label)
}

/// Whether the summary offers to correct `part`: the delivery fee only for a shop that
/// delivers, and the PromptPay ID only while it is asked for.
fn offered(config: &Config, draft: &ShopFlow, part: Step) -> bool {
    match part {
        Step::DeliveryFee => draft.delivers(),
        Step::PromptPay => asks_promptpay(config),
        Step::Summary => false,
        _ => true,
    }
}

/// The buttons that correct each part the summary offers.
fn parts(config: &Config, draft: &ShopFlow) -> Vec<Action> {
    Step::PARTS
        .into_iter()
        .filter(|&part| offered(config, draft, part))
        .map(|part| {
            let postback = Postback::new(PART)
                .with_name(WHICH, part.key())
                .with_int(REVISION, draft.revision);
            postback.button(part.label().clone())
        })
        .collect()
}

/// Why `step` didn't take an answer, before its choices; `None` when it is enough to ask again.
fn why(turn: &Turn<'_>, draft: &ShopFlow, step: Step, refused: Refused) -> Option<Message> {
    let templates = turn.templates;
    let none = none_label(draft, step);
    Some(match refused {
        Refused::TooLong(max) => templates.text_too_long(max),
        Refused::NotAPhone => templates.shop_phone_invalid(),
        Refused::NotAPromptPayId => templates.promptpay_invalid(none.unwrap_or(&labels::SKIP)),
        Refused::NotAnAmount(e) => Message::text(templates.number_problem(e)),
        Refused::TooMuch(max) => templates.amount_too_large(max),
        Refused::NoPickup => templates.pickup_needed(),
        Refused::NotAPhoto => templates.photo_expected(none.unwrap_or(&labels::SKIP)),
        Refused::NotALocation => templates.shop_location_expected(none),
        Refused::Photo(rejected) => Message::text(templates.image_rejected(rejected)),
        Refused::Empty | Refused::NotAChoice => return None,
    })
}

/// What asks for `step` of `draft`, for `user` as they are now.
fn prompt(
    turn: &Turn<'_>,
    config: &Config,
    user: &user::Model,
    draft: &ShopFlow,
    step: Step,
) -> Vec<Message> {
    let templates = turn.templates;
    // What was given before is shown only to correct it
    let current = |value: Option<String>| value.filter(|_| draft.editing);
    let none = none_label(draft, step);
    let ask = match step {
        Step::Name => templates.ask_shop_name(NAME_CHARS, current(draft.name.clone()).as_deref()),
        Step::Description => templates.ask_shop_description(
            DESCRIPTION_CHARS,
            current(draft.description.clone()).as_deref(),
        ),
        Step::Photo => templates.ask_shop_photo(draft.photo.is_some()),
        Step::PickupAddress => templates.ask_pickup_address(
            PICKUP_ADDRESS_CHARS,
            home(user).is_some(),
            none,
            current(draft.pickup_address.clone()).as_deref(),
        ),
        Step::PickupLocation => templates.ask_pickup_location(
            home_point(user).is_some(),
            none,
            draft.editing && draft.pickup_location.is_some(),
        ),
        Step::Phone => templates.ask_shop_phone(
            user.phone.is_some(),
            current(draft.phone.as_deref().map(thai::format_phone)).as_deref(),
        ),
        Step::Fulfillment => templates.ask_fulfillment(
            current(draft.fulfillment.map(|f| f.label().as_str().to_owned())).as_deref(),
        ),
        Step::DeliveryFee => templates.ask_delivery_fee(
            MAX_DELIVERY_FEE,
            current(Some(draft.delivery_fee.to_string())).as_deref(),
        ),
        Step::MinimumOrder => templates.ask_minimum_order(
            MAX_MINIMUM_ORDER,
            current(draft.minimum_order.map(|m| m.to_string())).as_deref(),
        ),
        Step::PromptPay => templates.ask_promptpay(
            promptpay_phone(draft).is_some(),
            none.unwrap_or(&labels::SKIP),
            current(
                draft
                    .promptpay_id
                    .as_deref()
                    .map(payments::format_promptpay_id),
            )
            .as_deref(),
        ),
        Step::OpeningHours => templates.ask_opening_hours(
            OPENING_HOURS_CHARS,
            none.unwrap_or(&labels::SKIP),
            current(draft.opening_hours.clone()).as_deref(),
        ),
        Step::Summary => return summary(turn, config, draft),
    };
    with_choices(vec![ask], choices(user, draft, step))
}

/// The summary of the application, with the buttons that send it or correct a part.
fn summary(turn: &Turn<'_>, config: &Config, draft: &ShopFlow) -> Vec<Message> {
    let approval = config.shop.require_approval;
    let submit = if approval {
        &labels::SUBMIT_APPLICATION
    } else {
        &labels::OPEN_NOW
    };
    let button = |action, label: &Label| {
        let postback = Postback::new(action).with_int(REVISION, draft.revision);
        postback.button(label.clone())
    };
    let buttons = vec![button(SUBMIT, submit), button(EDIT, &labels::EDIT)];
    let photo = photo_url(config, draft.photo.map(Photo::id));
    let card = draft_card(draft, photo.as_ref(), buttons);
    turn.templates.shop_summary(&card, approval, submit)
}

/// The Quick Replies of `step`, besides the cancel button the flow adds (FR-UI-04).
fn choices(user: &user::Model, draft: &ShopFlow, step: Step) -> Vec<Action> {
    let message = |label: &Label| Action::message(label.clone(), label.as_str());
    let typed = |text: String| Action::message(Label::fit(text.as_str()), text);
    let mut choices = match step {
        Step::Name | Step::Description | Step::OpeningHours | Step::Summary => Vec::new(),
        Step::Photo => vec![
            Action::Camera {
                label: labels::TAKE_PHOTO.clone(),
            },
            Action::CameraRoll {
                label: labels::CHOOSE_PHOTO.clone(),
            },
        ],
        Step::PickupAddress => home(user)
            .map(|_| message(&labels::USE_HOME_ADDRESS))
            .into_iter()
            .collect(),
        Step::PickupLocation => {
            let share = Action::Location {
                label: labels::SHARE_LOCATION.clone(),
            };
            let home = home_point(user).map(|_| message(&labels::USE_HOME_LOCATION));
            std::iter::once(share).chain(home).collect()
        }
        Step::Phone => user
            .phone
            .as_deref()
            .map(|phone| typed(thai::format_phone(phone)))
            .into_iter()
            .collect(),
        Step::Fulfillment => Fulfillment::ALL
            .iter()
            .map(|&(_, label)| message(label))
            .collect(),
        Step::DeliveryFee => vec![message(&labels::NO_DELIVERY_FEE)],
        Step::MinimumOrder => vec![message(&labels::NO_MINIMUM)],
        Step::PromptPay => promptpay_phone(draft)
            .map(|phone| typed(thai::format_phone(phone)))
            .into_iter()
            .collect(),
    };
    let none = none_label(draft, step).filter(|_| step != Step::MinimumOrder);
    choices.extend(none.map(message));
    if draft.editing && draft.has(step) && step != Step::Summary {
        choices.push(message(&labels::KEEP));
    }
    choices
}

/// The shop's phone, if it is a mobile number, which PromptPay takes.
fn promptpay_phone(draft: &ShopFlow) -> Option<&str> {
    draft.phone.as_deref().filter(|phone| phone.len() == 10)
}

/// Sends the application (FR-SHOP-01, FR-SHOP-03): the user's shop, made or applied for again,
/// `PENDING` for the admins, who are alerted, or `APPROVED` at once while `shop.requireApproval`
/// is off, unless moderation finds it suspicious (FR-PROD-09). A user who has a shop other than
/// a turned-down one can't apply (FR-SHOP-02).
async fn submit(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    mut draft: ShopFlow,
) -> Result<Vec<Message>, DbErr> {
    if let Some(step) = draft.missing() {
        let ask = prompt(turn, config, &turn.actor.user, &draft, step);
        return flow::answered(turn, Answer::Next(Session { step, draft }, ask)).await;
    }
    let user_id = turn.actor.user.id;
    // Before anything is locked, as the AI may take a while
    let flag = moderation::check(turn.db, config, ai, &subject(&draft)).await?;
    // The shop and its photo change together, or not at all
    let txn = turn.db.begin().await?;
    let existing = shop::Entity::find()
        .filter(shop::Column::OwnerId.eq(user_id))
        .lock_exclusive()
        .one(&txn)
        .await?;
    if let Some(shop) = existing
        .as_ref()
        .filter(|shop| shop.status != shop::Status::Rejected)
    {
        txn.rollback().await?;
        info!(shop_id = shop.id, "a second shop applied for; refused");
        let standing = standing(turn, config, shop).await?;
        return flow::answered(turn, Answer::<ShopFlow>::Done(standing)).await;
    }

    // Found suspicious, it waits for an admin even if no approval is asked for
    let status = if config.shop.require_approval || flag.is_some() {
        shop::Status::Pending
    } else {
        shop::Status::Approved
    };
    let application = shop::ActiveModel {
        status: Set(status),
        rejection_reason: Set(None),
        updated_at: Set(turn.now),
        ..details(&draft)
    };
    let shop = match existing {
        None => {
            shop::ActiveModel {
                owner_id: Set(user_id),
                is_open: Set(true),
                // Shown to buyers with an active order unless the seller says otherwise
                // (FR-SHOP-08)
                show_phone: Set(true),
                created_at: Set(turn.now),
                ..application
            }
            .insert(&txn)
            .await?
        }
        Some(rejected) => shop::Entity::update_many()
            .set(application)
            .filter(shop::Column::Id.eq(rejected.id))
            .exec_with_returning(&txn)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| DbErr::RecordNotFound(format!("shop {}", rejected.id)))?,
    };
    if !keep_photo(&txn, user_id, shop.id, draft.photo).await? {
        // The shop stays as it was
        txn.rollback().await?;
        let ask = photo_lost(turn, config, &mut draft);
        let session = Session {
            step: Step::Photo,
            draft,
        };
        return flow::answered(turn, Answer::Next(session, ask)).await;
    }
    txn.commit().await?;

    let templates = turn.templates;
    let mut messages = if shop.status == shop::Status::Pending {
        let photo = photo_url(config, images::first(turn.db, Owner::Shop(shop.id)).await?);
        let alert = approvals::alert(templates, &shop, &turn.actor.user, photo.as_ref());
        turn.notify(Kind::ShopApplication, To::Admins, vec![alert]);
        if let Some(flag) = &flag {
            moderation::hold(turn, Held::Application(&shop), flag).await?;
        }
        info!(shop_id = shop.id, held = flag.is_some(), "shop applied for");
        vec![templates.application_sent(&shop.name)]
    } else {
        let entry = Entry {
            actor_id: None,
            action: audit::Action::ShopApproved,
            target: Target::Shop(shop.id),
            reason: Some("shop.requireApproval is false".to_owned()),
            details: audit::details([("name", json!(shop.name))]),
        };
        audit::record(turn.db, turn.now, [entry]).await?;
        turn.roles_changed(user_id);
        info!(shop_id = shop.id, "shop opened without approval");
        vec![templates.shop_opened(&shop.name)]
    };
    // With "ร้านของฉัน", and the seller's functions if it opened
    let actor = Actor::load(turn.db, user_id).await?;
    messages.push(menu::main_menu(actor.as_ref().unwrap_or(&turn.actor)));
    flow::answered(turn, Answer::<ShopFlow>::Done(messages)).await
}

/// What moderation checks of the shop as `draft` has it, which has every part the shop can't do
/// without: its name and description, and its photo if new (FR-PROD-09).
fn subject(draft: &ShopFlow) -> Subject<'_> {
    let photo = match draft.photo {
        Some(Photo::New(id)) => Some(id),
        Some(Photo::Current(_)) | None => None,
    };
    let name = draft.name.as_deref().expect("not missing");
    let description = draft.description.as_deref().expect("not missing");
    Subject::shop(name, description, photo)
}

/// What the shop's details become as `draft` has them, which has every part the shop can't do
/// without: what an application sends, and what "ตั้งค่าร้าน" saves.
fn details(draft: &ShopFlow) -> shop::ActiveModel {
    let fulfillment = draft.fulfillment.expect("not missing");
    let location = draft.pickup_location;
    shop::ActiveModel {
        name: Set(draft.name.clone().expect("not missing")),
        description: Set(draft.description.clone().expect("not missing")),
        opening_hours: Set(draft.opening_hours.clone()),
        offers_pickup: Set(fulfillment.pickup()),
        offers_delivery: Set(fulfillment.delivery()),
        delivery_fee: Set(draft.delivery_fee),
        minimum_order: Set(draft.minimum_order),
        promptpay_id: Set(draft.promptpay_id.clone()),
        phone: Set(draft.phone.clone()),
        pickup_address: Set(draft.pickup_address.clone()),
        pickup_latitude: Set(location.map(|point| point.latitude)),
        pickup_longitude: Set(location.map(|point| point.longitude)),
        ..Default::default()
    }
}

/// Makes `photo` the only one of shop `shop_id`, adopting it from user `user_id`'s drafts if it
/// was sent in the flow, or removes the shop's if there is none. `false`, changing nothing, if
/// that draft is gone: swept, the flow having been idle too long.
async fn keep_photo(
    db: &impl ConnectionTrait,
    user_id: i64,
    shop_id: i64,
    photo: Option<Photo>,
) -> Result<bool, DbErr> {
    let keep = match photo {
        Some(Photo::New(id)) => {
            if !images::adopt(db, id, user_id, Owner::Shop(shop_id)).await? {
                return Ok(false);
            }
            Some(id)
        }
        Some(Photo::Current(id)) => Some(id),
        None => None,
    };
    images::keep_only(db, Owner::Shop(shop_id), keep).await?;
    Ok(true)
}

/// The photo sent earlier is gone: `draft` without it, and what asks for it again before the
/// shop can be saved.
fn photo_lost(turn: &Turn<'_>, config: &Config, draft: &mut ShopFlow) -> Vec<Message> {
    draft.photo = None;
    draft.revision += 1;
    let mut ask = vec![turn.templates.photo_lost()];
    ask.extend(prompt(turn, config, &turn.actor.user, draft, Step::Photo));
    ask
}

/// The `/img/p` link of photo `id`, if there is one.
fn photo_url(config: &Config, id: Option<Uuid>) -> Option<Url> {
    id.map(|id| images::product_image_url(&config.app.base_url, id))
}

/// Whether `shop` takes orders now: `APPROVED`, and open (FR-SHOP-05, FR-SHOP-10). Only then can
/// its products go in a [cart](crate::orders::cart) or be checked out (#32); closed, they stay
/// visible. Held for review, it takes none, whether open or not (FR-PROD-09).
pub fn takes_orders(shop: &shop::Model) -> bool {
    shop.status == shop::Status::Approved && shop.is_open
}

/// What `shop`'s card shows, with `photo` and `buttons`: to its owner, with how it is run once
/// approved, or, given who applied, to the admins, who see only whether it has a PromptPay ID
/// (NFR-PRIV-04).
pub fn card<'a>(
    shop: &'a shop::Model,
    photo: Option<&'a Url>,
    applicant: Option<Applicant<'a>>,
    buttons: Vec<Action>,
) -> ShopCard<'a> {
    let promptpay = if applicant.is_some() {
        PromptPay::Given(shop.promptpay_id.is_some())
    } else {
        PromptPay::Number(shop.promptpay_id.as_deref())
    };
    let runs = matches!(shop.status, shop::Status::Approved | shop::Status::Held);
    let running = (applicant.is_none() && runs).then_some(Running {
        is_open: shop.is_open,
        held: shop.status == shop::Status::Held,
        opening_hours: shop.opening_hours.as_deref(),
        show_phone: shop.show_phone,
    });
    ShopCard {
        name: &shop.name,
        description: &shop.description,
        photo,
        pickup_address: shop.pickup_address.as_deref(),
        pickup_location: shop.pickup_latitude.is_some() && shop.pickup_longitude.is_some(),
        phone: shop.phone.as_deref(),
        offers_pickup: shop.offers_pickup,
        offers_delivery: shop.offers_delivery,
        delivery_fee: shop.delivery_fee,
        minimum_order: shop.minimum_order,
        promptpay,
        running,
        applicant,
        buttons,
    }
}

/// `shop`'s card as buyers see it, with `photo` (FR-SHOP-09): its "ดูสินค้า" lists the shop's
/// products (#28).
pub fn front<'a>(shop: &'a shop::Model, photo: Option<&'a Url>) -> ShopFront<'a> {
    let products = Postback::new(browse::SHOP).with_int(browse::SHOP_ID, shop.id);
    ShopFront {
        name: &shop.name,
        description: &shop.description,
        photo,
        is_open: takes_orders(shop),
        opening_hours: shop.opening_hours.as_deref(),
        offers_pickup: shop.offers_pickup,
        offers_delivery: shop.offers_delivery,
        delivery_fee: shop.delivery_fee,
        minimum_order: shop.minimum_order,
        products: products.button(labels::VIEW_PRODUCTS.clone()),
    }
}

/// What the summary's card shows of `draft`, which has every part it can't do without.
fn draft_card<'a>(
    draft: &'a ShopFlow,
    photo: Option<&'a Url>,
    buttons: Vec<Action>,
) -> ShopCard<'a> {
    let fulfillment = draft.fulfillment.unwrap_or(Fulfillment::Pickup);
    ShopCard {
        name: draft.name.as_deref().unwrap_or_default(),
        description: draft.description.as_deref().unwrap_or_default(),
        photo,
        pickup_address: draft.pickup_address.as_deref(),
        pickup_location: draft.pickup_location.is_some(),
        phone: draft.phone.as_deref(),
        offers_pickup: fulfillment.pickup(),
        offers_delivery: fulfillment.delivery(),
        delivery_fee: draft.delivery_fee,
        minimum_order: draft.minimum_order,
        promptpay: PromptPay::Number(draft.promptpay_id.as_deref()),
        running: None,
        applicant: None,
        buttons,
    }
}
