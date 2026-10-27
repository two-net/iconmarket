//! The address (FR-ONB-04): zone, house number and an optional landmark note, with an optional
//! phone number and shared location (NFR-PRIV-04). A guided flow asks for it the first time it
//! is needed, at the user's first checkout or shop application, and to correct it from
//! "ข้อมูลของฉัน" (FR-ONB-05).
//!
//! It is a sub-flow: whoever needs the address [`start`]s it with what to ask for ([`Ask`]) and
//! where to go once that is saved ([`Then`]). Nothing is saved before its last step is answered,
//! so cancelling it (FR-UI-04) or letting it expire (FR-UI-15) changes nothing. What the user
//! types is never logged (NFR-PRIV-05).

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::access::Denied;
use super::profile;
use crate::catalog::shops;
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Location, Turn};
use crate::line::{Action, Message, QuickReply, limits};
use crate::notifications::templates::{self, labels};
use crate::orders::checkout;
use crate::persistence::entity::user;
use crate::thai::{self, Label};

/// The action of "ดูเพิ่มเติม" under a long list of zones (FR-LINE-11); its `page` is the page
/// to show.
pub const MORE_ZONES: &str = "address.zones";
const PAGE: &str = "page";

/// The longest zone typed when `village.zones` lists none
const ZONE_CHARS: usize = 50;
const HOUSE_NUMBER_CHARS: usize = 30;
/// FR-ONB-04
const LANDMARK_NOTE_CHARS: usize = 100;
/// Zones on a page of Quick Replies, leaving room for "ดูเพิ่มเติม" and the cancel button
const ZONES_PER_PAGE: usize = limits::QUICK_REPLY_ITEMS - 2;

/// What the flow asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    /// Everything, the first time the address is needed: zone, house number, landmark note,
    /// phone and location
    All,
    /// Zone, house number and landmark note
    Address,
    Phone,
    Location,
}

/// Where the user goes once the flow has saved what it asked for. Each module that needs the
/// address adds its own: shop applications (#23) and checkout (#32).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Then {
    /// Back to "ข้อมูลของฉัน" (FR-ONB-05)
    MyData,
    /// On to "เปิดร้าน" (FR-SHOP-01)
    OpenShop,
    /// Back to checking out shop `shop_id` (FR-CART-04): from the first step, or, if the
    /// address was changed to `deliver` to, from the payment method
    Checkout { shop_id: i64, deliver: bool },
}

/// The flow's draft: what it asks for, where it goes next, and the answers so far. An answer
/// left `None` once its step is past means "none": no landmark note, or no phone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddressFlow {
    ask: Ask,
    then: Then,
    zone: Option<String>,
    house_number: Option<String>,
    landmark_note: Option<String>,
    phone: Option<String>,
    location: Option<Point>,
}

/// A shared location, without the address text LINE adds to it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Zone,
    HouseNumber,
    Landmark,
    Phone,
    Location,
}

impl Flow for AddressFlow {
    const NAME: &'static str = "address";
    type Step = Step;
}

impl Ask {
    fn first_step(self) -> Step {
        match self {
            Self::All | Self::Address => Step::Zone,
            Self::Phone => Step::Phone,
            Self::Location => Step::Location,
        }
    }

    /// The step after `step`; `None` after the last.
    fn after(self, step: Step) -> Option<Step> {
        match (self, step) {
            (Self::All | Self::Address, Step::Zone) => Some(Step::HouseNumber),
            (Self::All | Self::Address, Step::HouseNumber) => Some(Step::Landmark),
            (Self::All, Step::Landmark) => Some(Step::Phone),
            (Self::All, Step::Phone) => Some(Step::Location),
            _ => None,
        }
    }
}

impl Then {
    /// What the user is shown once `user`, as just saved, has their address.
    async fn resume(
        self,
        turn: &Turn<'_>,
        config: &Config,
        user: &user::Model,
    ) -> Result<Vec<Message>, DbErr> {
        match self {
            Self::MyData => profile::show(turn, config, user).await,
            Self::OpenShop => shops::open(turn, config, user).await,
            Self::Checkout { shop_id, deliver } => {
                checkout::resume(turn, config, user, shop_id, deliver).await
            }
        }
    }
}

/// Whether `user` has an address to deliver to: a zone and a house number. Phone and location
/// are optional (NFR-PRIV-04).
pub fn has_address(user: &user::Model) -> bool {
    user.zone.is_some() && user.house_number.is_some()
}

/// Starts asking for `ask`, in place of any flow in progress, and goes on to `then` once it is
/// saved.
pub async fn start(
    turn: &Turn<'_>,
    config: &Config,
    ask: Ask,
    then: Then,
) -> Result<Vec<Message>, DbErr> {
    let draft = AddressFlow {
        ask,
        then,
        zone: None,
        house_number: None,
        landmark_note: None,
        phone: None,
        location: None,
    };
    let step = ask.first_step();
    let prompt = prompt(turn, config, &draft, step, 0);
    flow::start(turn, Session { step, draft }, prompt).await
}

/// The answer to the step the user is on (FR-UI-12 step 3). `None` when the step doesn't take
/// this kind of input, such as a photo, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    session: Session<AddressFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Session { step, mut draft } = session;
    let removable = removable(turn, &draft, step);
    let taken = match (step, input) {
        // LINE's address text for it isn't needed, so isn't kept (NFR-PRIV-04)
        (
            Step::Location,
            &Input::Location(Location {
                latitude,
                longitude,
                address: _,
            }),
        ) => {
            draft.location = Some(Point {
                latitude,
                longitude,
            });
            Ok(())
        }
        (_, Input::Text(text)) => take(step, &mut draft, &config.village.zones, text),
        _ => return Ok(None),
    };
    if let Err(refused) = taken {
        let templates = turn.templates;
        let why = match refused {
            Refused::Empty => None,
            Refused::TooLong(max) => Some(templates.text_too_long(max)),
            Refused::UnknownZone => Some(templates.zone_unknown()),
            Refused::NotAPhone => Some(templates.phone_invalid(removable)),
            Refused::NotALocation => Some(templates.location_expected(removable)),
        };
        // Why, with the step's choices; or, for nothing at all, the step's question again
        let again = match why {
            Some(why) => with_choices(vec![why], choices(config, step, removable, 0)),
            None => prompt(turn, config, &draft, step, 0),
        };
        return flow::answered(turn, Answer::<AddressFlow>::Again(again))
            .await
            .map(Some);
    }
    let messages = match draft.ask.after(step) {
        Some(next) => {
            let prompt = prompt(turn, config, &draft, next, 0);
            let session = Session { step: next, draft };
            flow::answered(turn, Answer::Next(session, prompt)).await?
        }
        None => finish(turn, config, draft).await?,
    };
    Ok(Some(messages))
}

/// A tap on one of the flow's buttons: "ดูเพิ่มเติม" under the zones shows the next page, while
/// the zone is still asked for.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    let session = turn
        .flow
        .as_ref()
        .and_then(|flow| flow.read::<AddressFlow>());
    let page = postback
        .int(PAGE)
        .and_then(|page| usize::try_from(page).ok());
    match (postback.action(), session, page) {
        (
            MORE_ZONES,
            Some(Session {
                step: Step::Zone,
                draft,
            }),
            Some(page),
        ) => {
            let prompt = prompt(turn, config, &draft, Step::Zone, page);
            flow::answered(turn, Answer::<AddressFlow>::Again(prompt)).await
        }
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// Why an answer wasn't taken; the step asks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// Nothing but spaces
    Empty,
    /// Longer than the step takes, in characters
    TooLong(usize),
    /// Not one of `village.zones`
    UnknownZone,
    NotAPhone,
    /// Text where a shared location was asked for
    NotALocation,
}

/// Takes `text` as the answer to `step` into `draft`. `zones` are `village.zones`.
fn take(step: Step, draft: &mut AddressFlow, zones: &[String], text: &str) -> Result<(), Refused> {
    let text = text.trim();
    let none = is_none(text);
    match step {
        Step::Zone => draft.zone = Some(zone(text, zones)?),
        Step::HouseNumber => {
            draft.house_number = Some(thai::arabic_digits(bounded(text, HOUSE_NUMBER_CHARS)?))
        }
        Step::Landmark if none => draft.landmark_note = None,
        Step::Landmark => {
            draft.landmark_note = Some(bounded(text, LANDMARK_NOTE_CHARS)?.to_owned())
        }
        Step::Phone if none => draft.phone = None,
        Step::Phone => draft.phone = Some(thai::parse_phone(text).ok_or(Refused::NotAPhone)?),
        Step::Location if none => draft.location = None,
        Step::Location => return Err(Refused::NotALocation),
    }
    Ok(())
}

/// The zone `text` names: one of `zones`, written as listed, if `village.zones` lists any;
/// else as typed. A zone matches whatever the spaces, the case of its Latin letters, or the
/// digits it is typed in: `ซอย๓` is `ซอย 3`.
fn zone(text: &str, zones: &[String]) -> Result<String, Refused> {
    if zones.is_empty() {
        return bounded(text, ZONE_CHARS).map(str::to_owned);
    }
    if text.is_empty() {
        return Err(Refused::Empty);
    }
    let normal = |zone: &str| -> String {
        thai::arabic_digits(zone)
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let typed = normal(text);
    zones
        .iter()
        .find(|zone| normal(zone) == typed)
        .cloned()
        .ok_or(Refused::UnknownZone)
}

/// `text`, if it is neither empty nor longer than `max` characters.
fn bounded(text: &str, max: usize) -> Result<&str, Refused> {
    match text.chars().count() {
        0 => Err(Refused::Empty),
        chars if chars > max => Err(Refused::TooLong(max)),
        _ => Ok(text),
    }
}

/// Whether `text` goes without an optional value: "ข้าม", or "ลบ…" when correcting one.
fn is_none(text: &str) -> bool {
    [
        &labels::SKIP,
        &labels::REMOVE_PHONE,
        &labels::REMOVE_LOCATION,
    ]
    .iter()
    .any(|label| label.as_str() == text)
}

/// Whether the flow corrects what the user gave before, rather than asking for it the first
/// time: their current values are then shown, and optional ones can be removed.
fn correcting(draft: &AddressFlow) -> bool {
    draft.ask != Ask::All
}

/// Whether `step` corrects an optional value the user gave, which they can then remove.
fn removable(turn: &Turn<'_>, draft: &AddressFlow, step: Step) -> bool {
    let user = &turn.actor.user;
    correcting(draft)
        && match step {
            Step::Phone => user.phone.is_some(),
            Step::Location => user.latitude.is_some() && user.longitude.is_some(),
            Step::Zone | Step::HouseNumber | Step::Landmark => false,
        }
}

/// What asks for `step`, its zones from page `page`.
fn prompt(
    turn: &Turn<'_>,
    config: &Config,
    draft: &AddressFlow,
    step: Step,
    page: usize,
) -> Vec<Message> {
    let (templates, user) = (turn.templates, &turn.actor.user);
    // What the user gave before is shown only to correct it
    let shown = correcting(draft);
    let removable = removable(turn, draft, step);
    let ask = match step {
        Step::Zone => {
            let offered = !config.village.zones.is_empty();
            let current = user.zone.as_deref().filter(|_| shown);
            templates.ask_zone(draft.ask == Ask::All, offered, current)
        }
        Step::HouseNumber => {
            templates.ask_house_number(user.house_number.as_deref().filter(|_| shown))
        }
        Step::Landmark => {
            let current = user.landmark_note.as_deref().filter(|_| shown);
            templates.ask_landmark(LANDMARK_NOTE_CHARS, current)
        }
        Step::Phone => {
            let phone = user.phone.as_deref().map(thai::format_phone);
            templates.ask_phone(phone.as_deref().filter(|_| removable))
        }
        Step::Location => templates.ask_location(removable),
    };
    with_choices(vec![ask], choices(config, step, removable, page))
}

/// The Quick Replies of `step`: page `page` of the zones, or how to go without an optional
/// value, or remove it when `removable`. The flow adds the cancel button (FR-UI-04).
fn choices(config: &Config, step: Step, removable: bool, page: usize) -> Vec<Action> {
    let message = |label: &Label| Action::message(label.clone(), label.as_str());
    match step {
        Step::Zone => {
            let zones = &config.village.zones;
            let pages = zones.len().div_ceil(ZONES_PER_PAGE);
            let page = if page < pages { page } else { 0 };
            let zone = |zone: &String| Action::message(Label::fit(zone.as_str()), zone.as_str());
            let more = |next| {
                let more = Postback::new(MORE_ZONES).with_int(PAGE, next as i64);
                more.to_string()
            };
            templates::quick_reply_page(zones, page, ZONES_PER_PAGE, zone, more)
        }
        Step::HouseNumber => Vec::new(),
        Step::Landmark => vec![message(&labels::SKIP)],
        Step::Phone if removable => vec![message(&labels::REMOVE_PHONE)],
        Step::Phone => vec![message(&labels::SKIP)],
        Step::Location => {
            let none = if removable {
                &labels::REMOVE_LOCATION
            } else {
                &labels::SKIP
            };
            let share = Action::Location {
                label: labels::SHARE_LOCATION.clone(),
            };
            vec![share, message(none)]
        }
    }
}

/// `messages` with `choices` as the Quick Replies of the last.
fn with_choices(mut messages: Vec<Message>, choices: Vec<Action>) -> Vec<Message> {
    if choices.is_empty() {
        return messages;
    }
    if let Some(last) = messages.pop() {
        messages.push(last.with_quick_reply(QuickReply::new(choices)));
    }
    messages
}

/// Saves what `draft` asked for, ends the flow, and goes on to where it leads.
async fn finish(
    turn: &Turn<'_>,
    config: &Config,
    draft: AddressFlow,
) -> Result<Vec<Message>, DbErr> {
    let user = save(turn, &draft).await?;
    let templates = turn.templates;
    let saved = match draft.ask {
        Ask::All | Ask::Address => {
            let zone = user.zone.as_deref().unwrap_or_default();
            let house_number = user.house_number.as_deref().unwrap_or_default();
            let line = templates::address_line(zone, house_number, user.landmark_note.as_deref());
            templates.address_saved(&line)
        }
        Ask::Phone => {
            let phone = user.phone.as_deref().map(thai::format_phone);
            templates.phone_saved(phone.as_deref())
        }
        Ask::Location => templates.location_saved(draft.location.is_some()),
    };
    // Ended before going on, as where it leads may start a flow of its own
    let mut messages = flow::answered(turn, Answer::<AddressFlow>::Done(vec![saved])).await?;
    messages.extend(draft.then.resume(turn, config, &user).await?);
    Ok(messages)
}

/// Stores what `draft` asked for as the user's, and returns them as they now are.
async fn save(turn: &Turn<'_>, draft: &AddressFlow) -> Result<user::Model, DbErr> {
    let mut user = turn.actor.user.clone();
    let address = matches!(draft.ask, Ask::All | Ask::Address);
    let phone = matches!(draft.ask, Ask::All | Ask::Phone);
    let location = matches!(draft.ask, Ask::All | Ask::Location);
    let mut row = user::ActiveModel {
        updated_at: Set(turn.now),
        ..Default::default()
    };
    if address {
        user.zone.clone_from(&draft.zone);
        user.house_number.clone_from(&draft.house_number);
        user.landmark_note.clone_from(&draft.landmark_note);
        row.zone = Set(user.zone.clone());
        row.house_number = Set(user.house_number.clone());
        row.landmark_note = Set(user.landmark_note.clone());
    }
    if phone {
        user.phone.clone_from(&draft.phone);
        row.phone = Set(user.phone.clone());
    }
    if location {
        user.latitude = draft.location.map(|point| point.latitude);
        user.longitude = draft.location.map(|point| point.longitude);
        row.latitude = Set(user.latitude);
        row.longitude = Set(user.longitude);
    }
    user.updated_at = turn.now;
    user::Entity::update_many()
        .set(row)
        .filter(user::Column::Id.eq(user.id))
        .exec(turn.db)
        .await?;
    // Which part only: the values are personal data (NFR-PRIV-05)
    info!(ask = ?draft.ask, "address saved");
    Ok(user)
}
