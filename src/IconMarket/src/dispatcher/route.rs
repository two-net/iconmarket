//! Routing (§3.1.3): who handles an event. Group events go to the village group digest, or
//! nowhere (FR-LINE-06). A user's message or tap goes, after the rate limit (NFR-SEC-05), to
//! the first of these that applies (FR-UI-12):
//!
//! 1. a postback, to the module owning its action;
//! 2. onboarding, until it is complete; only `#myid` and `ช่วยเหลือ` work besides;
//! 3. the user's guided flow, which takes the input as an answer (`ยกเลิก` always cancels);
//! 4. the global keywords;
//! 5. the AI assistant, if enabled;
//! 6. a short help text and the main menu.
//!
//! A photo no flow step takes goes on by FR-UI-14, and what the dispatcher can't take gets a
//! friendly reply with the main menu (FR-UI-09). Everything routed here is handled in the
//! event's transaction, which also queues what it notifies; its reply and notifications are
//! sent once that commits.
//!
//! Many destinations are built by later issues. Until then each is an arm here that logs which
//! issue builds it and answers with the reply the dispatcher gives when nothing else applies;
//! each issue replaces its arm with a call into its module.

use sea_orm::prelude::DateTimeUtc;
use sea_orm::{DatabaseTransaction, DbErr};
use tracing::{Span, debug, info, warn};

use super::event::{Content, Event, Kind, Location, Source};
use super::flow::{self, Flow};
use super::limit::{self, Verdict};
use super::menu::{self, Item};
use super::pending;
use super::postback::Postback;
use super::worker::Context;
use super::{Finished, Turn};
use crate::admin::approvals::{self, RejectionFlow};
use crate::admin::categories::{self, CategoryFlow};
use crate::ai::Feature;
use crate::catalog::browse;
use crate::catalog::products::manage::{self, EditFlow};
use crate::catalog::products::{self, ProductFlow};
use crate::catalog::shops::settings::{self, SettingsFlow};
use crate::catalog::shops::{self, ShopFlow};
use crate::line::Message;
use crate::notifications::Queued;
use crate::notifications::templates::labels;
use crate::payments;
use crate::persistence::entity::webhook_event;
use crate::users::access::{Actor, Denied};
use crate::users::address::{self, AddressFlow};
use crate::users::onboarding::{self, Said};
use crate::users::report::{self, ReportFlow};
use crate::users::{self, help, profile, roles};

/// Typed by the operator to get their own LINE user ID, for `admin.lineUserIds` (FR-ONB-08)
const MY_ID: &str = "#myid";
/// Typed by an admin in the village group to get its ID, for `line.villageGroupId` (FR-NOT-07)
const GROUP_ID: &str = "#groupid";
/// What the actions of onboarding's buttons start with (FR-ONB-02)
const ONBOARDING: &str = "onboarding.";
/// What the actions of the buttons of "ข้อมูลของฉัน" start with (FR-ONB-05)
const MY_DATA: &str = "my_data.";
/// What the actions of the address flow's buttons start with (FR-ONB-04)
const ADDRESS: &str = "address.";
/// What the actions of the buttons of "หมวดหมู่" start with (FR-ADM-05)
const CATEGORIES: &str = "categories.";
/// What the actions of a shop application's buttons start with (FR-SHOP-01)
const OPEN_SHOP: &str = "open_shop.";
/// What the actions of the buttons of "อนุมัติร้าน" start with (FR-ADM-02)
const APPROVALS: &str = "approvals.";

/// The answer to an event, sent once its transaction commits.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub token: String,
    /// The chat it answers, should it have to be pushed instead
    pub to: String,
    pub messages: Vec<Message>,
}

/// What handling an event leaves to send once its transaction commits.
#[derive(Debug, Default)]
pub struct Handled {
    pub reply: Option<Reply>,
    /// The notifications it queued (FR-NOT-04)
    pub pushes: Queued,
    /// The users whose rich menus to relink (FR-UI-10)
    pub relink: Vec<i64>,
}

/// What a user sent, as handed to the module that handles it.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Text(String),
    /// Its owner downloads it while the event is processed (FR-LINE-09), with
    /// [`images::receive`](crate::catalog::images::receive)
    Image {
        message_id: String,
    },
    Location(Location),
    Audio {
        message_id: String,
    },
    /// The data of the button tapped
    Postback(String),
    Unsupported,
}

impl Input {
    fn new(content: Content) -> Self {
        match content {
            Content::Text(text) => Self::Text(text),
            Content::Image { message_id } => Self::Image { message_id },
            Content::Location(location) => Self::Location(location),
            Content::Audio { message_id } => Self::Audio { message_id },
            Content::Unsupported => Self::Unsupported,
        }
    }

    /// What kind it is, for log lines, which never hold what the user sent (NFR-PRIV-05)
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Image { .. } => "image",
            Self::Location(_) => "location",
            Self::Audio { .. } => "audio",
            Self::Postback(_) => "postback",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Handles `event`, stored as `row`, at `now`. Everything it changes goes through `txn`, in which
/// what it notifies is queued too; the reply, if there is one, the notifications and the rich
/// menus of users whose roles changed are sent once `txn` commits.
pub async fn handle(
    context: &Context,
    txn: &DatabaseTransaction,
    row: &webhook_event::Model,
    event: Event,
    now: DateTimeUtc,
) -> Result<Handled, DbErr> {
    let Event {
        reply_token,
        source,
        kind,
    } = event;
    let Finished {
        messages,
        outbox,
        relink,
    } = match source {
        Source::User(line_user_id) => {
            user_event(context, txn, row, &line_user_id, kind, now).await?
        }
        Source::Group { .. } => {
            group_event(context, &kind);
            Finished::default()
        }
        // Multi-person chats, and events from no chat, are ignored
        Source::Room | Source::Unknown => Finished::default(),
    };
    let pushes = outbox.queue(txn, &context.config.notification, now).await?;
    let reply = match reply_token {
        Some(token) if !messages.is_empty() => Some(Reply {
            token,
            to: row.user_key.clone(),
            messages,
        }),
        _ => None,
    };
    Ok(Handled {
        reply,
        pushes,
        relink,
    })
}

/// An event in a user's one-to-one chat (FR-LINE-06): the reply, what it notifies, and the rich
/// menus to relink.
async fn user_event(
    context: &Context,
    txn: &DatabaseTransaction,
    row: &webhook_event::Model,
    line_user_id: &str,
    kind: Kind,
    now: DateTimeUtc,
) -> Result<Finished, DbErr> {
    let input = match kind {
        Kind::Follow => {
            let Some(turn) = load_turn(context, txn, line_user_id, now, true).await? else {
                return Ok(Default::default());
            };
            let messages = onboarding::followed(&turn, &context.config).await?;
            return Ok(turn.finish(messages));
        }
        // Without registering anyone: a user who never wrote has nothing to update
        Kind::Unfollow => {
            debug!(what = "unfollowing", issue = 36, "not built yet");
            return Ok(Default::default());
        }
        Kind::Message(content) => Input::new(content),
        Kind::Postback(data) => Input::Postback(data),
        Kind::Join | Kind::Leave | Kind::Other => return Ok(Default::default()),
    };

    let per_minute = context.config.limits.messages_per_user_per_minute;
    match limit::check(txn, row, per_minute).await? {
        Verdict::Allowed => {}
        Verdict::Notice => {
            info!(per_minute, "user over the rate limit; told to slow down");
            return Ok(Finished {
                messages: vec![context.templates.slow_down()],
                ..Finished::default()
            });
        }
        Verdict::Ignored => {
            debug!(per_minute, "user over the rate limit; event ignored");
            return Ok(Default::default());
        }
    }

    let Some(mut turn) = load_turn(context, txn, line_user_id, now, false).await? else {
        return Ok(Default::default());
    };
    let messages = route(context, &mut turn, input).await?;
    // With the notice of an expired flow (FR-UI-15)
    Ok(turn.finish(messages))
}

/// The turn of the user with `line_user_id`, registered on their first event (FR-ONB-01) and
/// loaded as they are now. Their LINE profile is fetched first when `follow`ing, and otherwise
/// at most once a day (FR-LINE-10). `None` if they can't be loaded, which a concurrent account
/// deletion could cause.
async fn load_turn<'a>(
    context: &'a Context,
    txn: &'a DatabaseTransaction,
    line_user_id: &str,
    now: DateTimeUtc,
    follow: bool,
) -> Result<Option<Turn<'a>>, DbErr> {
    let user = roles::register(txn, &context.config.admin, line_user_id, now).await?;
    if follow || users::profile_refresh_due(user.profile_refreshed_at, now) {
        match context.line.profile(line_user_id).await {
            Ok(profile) => users::store_profile(txn, user.id, profile, now).await?,
            // Tried again at the next event; the user is answered all the same
            Err(e) => warn!(error = %e, "cannot fetch the user's LINE profile"),
        }
    }
    let Some(actor) = Actor::load(txn, user.id).await? else {
        return Ok(None);
    };
    Span::current().record("user_id", actor.user.id);
    let session = &context.config.session;
    Turn::new(txn, &context.templates, session, actor, now)
        .await
        .map(Some)
}

/// A group event (FR-LINE-06): with the village group digest on (FR-NOT-07), `join`, `leave`
/// and `#groupid` are the digest's; everything else in groups is ignored.
fn group_event(context: &Context, kind: &Kind) {
    if !context.config.notification.group_digest {
        return;
    }
    let what = match kind {
        Kind::Join => "joining a group",
        Kind::Leave => "leaving a group",
        Kind::Message(Content::Text(text)) if text.trim() == GROUP_ID => GROUP_ID,
        _ => return,
    };
    debug!(what, issue = 48, "not built yet");
}

/// What the routing priority (FR-UI-12) found about the user and the features on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
    onboarded: bool,
    in_flow: bool,
    assistant: bool,
    voice: bool,
}

/// Where an input goes.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    /// To the module owning the postback's action
    Postback(Postback),
    /// Postback data no button of ours sends
    BadPostback,
    Onboarding,
    /// To the user's guided flow, which may not take this kind of input
    Flow,
    Keyword(Keyword),
    Assistant,
    Fallback,
    Image,
    Location,
    Voice,
    Unsupported,
}

/// The global keywords, matched exactly (FR-UI-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keyword {
    Menu,
    Cart,
    Orders,
    Help,
    Cancel,
    Restart,
    MyId,
}

impl Keyword {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let keyword = if flow::is_cancel(text) {
            Self::Cancel
        } else if text == MY_ID {
            Self::MyId
        } else {
            [
                (&labels::MENU, Self::Menu),
                (&labels::CART, Self::Cart),
                (&labels::ORDERS, Self::Orders),
                (&labels::HELP, Self::Help),
                (&labels::RESTART, Self::Restart),
            ]
            .into_iter()
            .find(|(label, _)| label.as_str() == text)?
            .1
        };
        Some(keyword)
    }
}

/// Whether a postback for `action` works before onboarding is complete: onboarding's own
/// buttons, help (FR-UI-12) and the rich menu's tabs, which only LINE acts on.
fn open_before_onboarding(action: &str) -> bool {
    action == Item::Help.action()
        || action == menu::SWITCH_TAB
        || action.starts_with(help::PREFIX)
        || action.starts_with(ONBOARDING)
}

/// Where `input` goes, by the routing priority (FR-UI-12).
fn decide(input: &Input, state: State) -> Step {
    let keyword = match input {
        Input::Text(text) => Keyword::parse(text),
        _ => None,
    };
    match input {
        // 1. A postback names its action
        Input::Postback(data) => match Postback::parse(data) {
            None => Step::BadPostback,
            Some(postback) if !state.onboarded && !open_before_onboarding(postback.action()) => {
                Step::Onboarding
            }
            Some(postback) => Step::Postback(postback),
        },
        // 2. Onboarding until it is complete
        _ if !state.onboarded => match keyword {
            Some(keyword @ (Keyword::MyId | Keyword::Help)) => Step::Keyword(keyword),
            _ => Step::Onboarding,
        },
        // 3. The flow takes any text but `ยกเลิก` as its answer, and photos and locations if
        // its step expects them
        Input::Text(_) if state.in_flow && keyword != Some(Keyword::Cancel) => Step::Flow,
        Input::Image { .. } | Input::Location(_) if state.in_flow => Step::Flow,
        // 4. – 6.
        Input::Text(_) => match keyword {
            Some(keyword) => Step::Keyword(keyword),
            None if state.assistant => Step::Assistant,
            None => Step::Fallback,
        },
        Input::Image { .. } => Step::Image,
        Input::Location(_) => Step::Location,
        // Understood like text once heard, flows included (FR-AI-15)
        Input::Audio { .. } if state.voice => Step::Voice,
        Input::Audio { .. } | Input::Unsupported => Step::Unsupported,
    }
}

/// Handles a user's message or tap in their turn.
async fn route(
    context: &Context,
    turn: &mut Turn<'_>,
    input: Input,
) -> Result<Vec<Message>, DbErr> {
    let config = &context.config;
    let state = State {
        onboarded: users::pending(&turn.actor.user, &config.privacy, &config.village).is_none(),
        in_flow: turn.flow.is_some(),
        assistant: context.ai.is_enabled(Feature::Assistant),
        voice: context.ai.is_enabled(Feature::Voice),
    };
    let mut step = decide(&input, state);
    if step == Step::Flow {
        if let Some(messages) = answer_flow(context, turn, &input).await? {
            return Ok(messages);
        }
        step = decide(
            &input,
            State {
                in_flow: false,
                ..state
            },
        );
    }
    debug!(input = input.kind(), step = step_name(&step), "routed");

    let templates = turn.templates;
    Ok(match step {
        Step::Postback(postback) => return tapped(context, turn, &postback).await,
        Step::BadPostback => vec![templates.denied(Denied::NotFound)],
        Step::Onboarding => {
            // Keywords other than help and `#myid` are never taken for a join code
            let said = match &input {
                Input::Text(text) if Keyword::parse(text).is_none() => Said::Text(text),
                _ => Said::Other,
            };
            return onboarding::answer(turn, config, said).await;
        }
        Step::Flow => unreachable!("decided again without the flow"),
        Step::Keyword(keyword) => return typed(turn, keyword).await,
        Step::Assistant => not_built("the AI assistant", 37, fallback(turn)),
        Step::Fallback => fallback(turn),
        Step::Image => return image(context, turn, &input).await,
        Step::Location => vec![templates.location_not_expected()],
        Step::Voice => not_built("voice messages", 50, fallback(turn)),
        Step::Unsupported => vec![templates.unsupported(), menu::main_menu(&turn.actor)],
    })
}

/// The name of `step`, for log lines, without the postback data it may hold.
fn step_name(step: &Step) -> &'static str {
    match step {
        Step::Postback(_) => "postback",
        Step::BadPostback => "bad postback",
        Step::Onboarding => "onboarding",
        Step::Flow => "flow",
        Step::Keyword(_) => "keyword",
        Step::Assistant => "assistant",
        Step::Fallback => "fallback",
        Step::Image => "image",
        Step::Location => "location",
        Step::Voice => "voice",
        Step::Unsupported => "unsupported",
    }
}

/// Hands `input` to the module whose flow the user is in (FR-UI-12 step 3), which answers its
/// current step. `None` when the step doesn't take this kind of input, so routing goes on
/// (FR-UI-14), or when this release has no such flow: it ends as if expired (FR-UI-15).
///
/// Each flow adds its arm, as its issue builds it: the address (#20), category names (#21),
/// problem reports (#22), shop applications and the reasons for turning one down (#23), shop
/// settings (#24), adding products (#25), changing them (#26) and checkout (#32).
async fn answer_flow(
    context: &Context,
    turn: &mut Turn<'_>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Some(stored) = turn.flow.clone() else {
        return Ok(None);
    };
    let config = &context.config;
    let answered = match stored.flow() {
        AddressFlow::NAME => match stored.read::<AddressFlow>() {
            Some(session) => Some(address::answer(turn, config, session, input).await),
            None => None,
        },
        CategoryFlow::NAME => match stored.read::<CategoryFlow>() {
            Some(session) => Some(categories::answer(turn, session, input).await),
            None => None,
        },
        ReportFlow::NAME => match stored.read::<ReportFlow>() {
            Some(session) => Some(report::answer(turn, session, input).await),
            None => None,
        },
        ShopFlow::NAME => match stored.read::<ShopFlow>() {
            Some(session) => Some(shops::answer(turn, config, &context.line, session, input).await),
            None => None,
        },
        RejectionFlow::NAME => match stored.read::<RejectionFlow>() {
            Some(session) => Some(approvals::answer(turn, config, session, input).await),
            None => None,
        },
        SettingsFlow::NAME => match stored.read::<SettingsFlow>() {
            Some(session) => {
                Some(settings::answer(turn, config, &context.line, session, input).await)
            }
            None => None,
        },
        ProductFlow::NAME => match stored.read::<ProductFlow>() {
            Some(session) => {
                Some(products::answer(turn, config, &context.line, session, input).await)
            }
            None => None,
        },
        EditFlow::NAME => match stored.read::<EditFlow>() {
            Some(session) => {
                Some(manage::answer(turn, config, &context.line, session, input).await)
            }
            None => None,
        },
        _ => None,
    };
    match answered {
        Some(answered) => answered,
        None => {
            debug!(
                flow = stored.flow(),
                input = input.kind(),
                "no such flow in this release, or a draft it can't read; ended"
            );
            turn.expire_flow().await?;
            Ok(None)
        }
    }
}

/// A postback (FR-UI-12 step 1, FR-UI-13): handled by the module owning its action, which
/// checks the user may still do it. Each module adds its actions as its issue builds it.
async fn tapped(
    context: &Context,
    turn: &Turn<'_>,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    match postback.action() {
        pending::CONFIRM => return pending::tapped(turn, postback).await,
        // LINE has switched to the tab's rich menu (FR-UI-10)
        menu::SWITCH_TAB => return Ok(Vec::new()),
        action if action.starts_with(ONBOARDING) => {
            return onboarding::tapped(turn, &context.config, postback).await;
        }
        action if action == profile::SHOW || action.starts_with(MY_DATA) => {
            return profile::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(ADDRESS) => {
            return address::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(CATEGORIES) => {
            return categories::tapped(turn, postback).await;
        }
        action if action.starts_with(OPEN_SHOP) => {
            return shops::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(APPROVALS) => {
            return approvals::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(settings::PREFIX) => {
            return settings::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(products::PREFIX) => {
            return products::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(manage::PREFIX) => {
            return manage::tapped(turn, &context.config, postback).await;
        }
        action if action.starts_with(help::PREFIX) => {
            return help::tapped(turn, &context.config, &context.ai, postback).await;
        }
        report::REPORT => return report::tapped(turn, postback).await,
        browse::CATEGORY => return Ok(not_built("browsing a category", 28, fallback(turn))),
        browse::SHOP => return Ok(not_built("a shop's products", 28, fallback(turn))),
        users::DELETE_ACCOUNT => {
            return Ok(not_built("deleting the account", 36, fallback(turn)));
        }
        _ => {}
    }
    match Item::from_action(postback.action()) {
        Some(item) => menu_item(context, turn, item, postback).await,
        None => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// A function of the main menu (FR-UI-05), by the module that owns it, or the issue that builds
/// it. `postback` is its button's, or that of "ดูเพิ่มเติม" under its list.
async fn menu_item(
    context: &Context,
    turn: &Turn<'_>,
    item: Item,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    use Item::*;
    let issue = match item {
        Buy => return browse::tapped(turn, postback).await,
        MyShop => return shops::tapped(turn, &context.config, postback).await,
        ApproveShops => return approvals::tapped(turn, &context.config, postback).await,
        Categories => return categories::tapped(turn, postback).await,
        Help => return help::tapped(turn, &context.config, &context.ai, postback).await,
        OpenClose | ShopSettings => {
            return settings::tapped(turn, &context.config, postback).await;
        }
        AddProduct => return products::tapped(turn, &context.config, postback).await,
        MyProducts => return manage::tapped(turn, &context.config, postback).await,
        Search => 28,
        Cart => 29,
        MyOrders | ShopOrders => 34,
        Sales => 39,
        ReviewQueue | Users => 31,
        Stats => 38,
        Announcements => 49,
    };
    Ok(not_built(item.action(), issue, fallback(turn)))
}

/// A global keyword (FR-UI-12 step 4).
async fn typed(turn: &Turn<'_>, keyword: Keyword) -> Result<Vec<Message>, DbErr> {
    Ok(match keyword {
        Keyword::Menu => vec![menu::main_menu(&turn.actor)],
        Keyword::Cancel => return flow::cancel(turn).await,
        // The AI assistant (#37) also clears the user's AI context here (FR-AI-11)
        Keyword::Restart => return flow::cancel(turn).await,
        Keyword::Cart => not_built("cart", 29, fallback(turn)),
        Keyword::Orders => not_built("orders", 34, fallback(turn)),
        Keyword::Help => return help::show(turn).await,
        Keyword::MyId => onboarding::my_id(turn),
    })
}

/// A photo no flow step took (FR-UI-14), sent as `input`. Whoever takes it downloads it while
/// the event is processed (FR-LINE-09). #30 has the AI draft a seller's listing from it, once
/// `ai.features.listing` is on.
async fn image(context: &Context, turn: &Turn<'_>, input: &Input) -> Result<Vec<Message>, DbErr> {
    let unexpected = || vec![turn.templates.image_not_expected()];
    if turn.actor.roles().seller
        && let Input::Image { message_id } = input
    {
        return products::photo_sent(turn, &context.config, &context.line, message_id).await;
    }
    if payments::awaiting_transfer(turn.db, turn.actor.user.id)
        .await?
        .is_some()
    {
        return Ok(not_built("payment slips", 41, unexpected()));
    }
    if context.ai.is_enabled(Feature::VisualSearch) {
        return Ok(not_built("visual search", 43, unexpected()));
    }
    Ok(unexpected())
}

/// The last resort (FR-UI-12 step 6): a short help text and the main menu.
fn fallback(turn: &Turn<'_>) -> Vec<Message> {
    vec![turn.templates.fallback(), menu::main_menu(&turn.actor)]
}

/// The reply of a function a later issue builds, `reply` until then.
fn not_built(what: &str, issue: u32, reply: Vec<Message>) -> Vec<Message> {
    debug!(what, issue, "not built yet");
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: State = State {
        onboarded: true,
        in_flow: false,
        assistant: false,
        voice: false,
    };

    fn text(text: &str) -> Input {
        Input::Text(text.to_owned())
    }

    fn image() -> Input {
        Input::Image {
            message_id: "1".to_owned(),
        }
    }

    fn location() -> Input {
        Input::Location(Location {
            latitude: 13.75,
            longitude: 100.5,
            address: None,
        })
    }

    fn audio() -> Input {
        Input::Audio {
            message_id: "2".to_owned(),
        }
    }

    fn postback(data: &str) -> Input {
        Input::Postback(data.to_owned())
    }

    fn tap(data: &str) -> Step {
        Step::Postback(Postback::parse(data).unwrap())
    }

    #[test]
    fn keywords_match_exactly() {
        for (text, keyword) in [
            ("เมนู", Keyword::Menu),
            (" เมนู ", Keyword::Menu),
            ("ตะกร้า", Keyword::Cart),
            ("ออเดอร์", Keyword::Orders),
            ("ช่วยเหลือ", Keyword::Help),
            ("ยกเลิก", Keyword::Cancel),
            ("เริ่มใหม่", Keyword::Restart),
            ("#myid", Keyword::MyId),
        ] {
            assert_eq!(Keyword::parse(text), Some(keyword), "{text:?}");
        }
        for text in [
            "เมนูอาหาร",
            "ขอเมนู",
            "#MyID",
            "#myid please",
            "ยกเลิกออเดอร์",
            "",
        ] {
            assert_eq!(Keyword::parse(text), None, "{text:?}");
        }
    }

    /// FR-UI-12 step 1: a postback goes to its action, before the flow and the keywords.
    #[test]
    fn a_postback_goes_to_its_action() {
        let in_flow = State {
            in_flow: true,
            ..ON
        };
        for state in [ON, in_flow] {
            assert_eq!(
                decide(&postback("browse?page=2"), state),
                tap("browse?page=2")
            );
            assert_eq!(
                decide(&postback("confirm?k=checkout"), state),
                tap("confirm?k=checkout")
            );
        }
        for data in ["", "Browse", "browse?q=ไข่", "browse?page=2&page=3"] {
            assert_eq!(decide(&postback(data), ON), Step::BadPostback, "{data:?}");
        }
    }

    /// FR-UI-12 step 2: before onboarding, only help, `#myid` and onboarding's own buttons work.
    #[test]
    fn onboarding_comes_first() {
        let new = State {
            onboarded: false,
            in_flow: true,
            assistant: true,
            voice: true,
        };
        assert_eq!(decide(&text("#myid"), new), Step::Keyword(Keyword::MyId));
        assert_eq!(decide(&text("ช่วยเหลือ"), new), Step::Keyword(Keyword::Help));
        assert_eq!(decide(&postback("help"), new), tap("help"));
        assert_eq!(decide(&postback("help.privacy"), new), tap("help.privacy"));
        assert_eq!(
            decide(&postback("onboarding.accept"), new),
            tap("onboarding.accept")
        );
        // A listed admin can have tabs before onboarding
        assert_eq!(decide(&postback("tab?to=admin"), new), tap("tab?to=admin"));
        for input in [
            text("เมนู"),
            text("ยกเลิก"),
            text("ขอไข่ 2 แผง"),
            image(),
            location(),
            audio(),
            Input::Unsupported,
            postback("buy"),
            postback("helpful"),
            postback("confirm?k=checkout"),
        ] {
            assert_eq!(decide(&input, new), Step::Onboarding, "{input:?}");
        }
        // Even malformed data gets nothing more than a refusal
        assert_eq!(decide(&postback("Browse"), new), Step::BadPostback);
    }

    /// FR-UI-12 step 3: the flow takes text before the keywords; `ยกเลิก` always cancels.
    #[test]
    fn a_flow_takes_the_answer_before_the_keywords() {
        let in_flow = State {
            in_flow: true,
            ..ON
        };
        for input in [
            text("เมนู"),
            text("ไข่ไก่"),
            text("#myid"),
            image(),
            location(),
        ] {
            assert_eq!(decide(&input, in_flow), Step::Flow, "{input:?}");
        }
        assert_eq!(
            decide(&text(" ยกเลิก "), in_flow),
            Step::Keyword(Keyword::Cancel)
        );
        assert_eq!(decide(&Input::Unsupported, in_flow), Step::Unsupported);
        assert_eq!(decide(&audio(), in_flow), Step::Unsupported);
        let voice = State {
            voice: true,
            ..in_flow
        };
        assert_eq!(decide(&audio(), voice), Step::Voice);
    }

    /// FR-UI-12 steps 4 – 6.
    #[test]
    fn then_the_keywords_the_assistant_and_the_fallback() {
        assert_eq!(decide(&text("เมนู"), ON), Step::Keyword(Keyword::Menu));
        assert_eq!(decide(&text("ยกเลิก"), ON), Step::Keyword(Keyword::Cancel));
        assert_eq!(decide(&text("เริ่มใหม่"), ON), Step::Keyword(Keyword::Restart));
        assert_eq!(decide(&text("ขอไข่ 2 แผง"), ON), Step::Fallback);
        let assistant = State {
            assistant: true,
            ..ON
        };
        assert_eq!(decide(&text("ขอไข่ 2 แผง"), assistant), Step::Assistant);
        assert_eq!(
            decide(&text("ตะกร้า"), assistant),
            Step::Keyword(Keyword::Cart)
        );
    }

    /// FR-UI-09, FR-UI-14: photos, locations and what can't be taken.
    #[test]
    fn other_input_goes_by_its_kind() {
        assert_eq!(decide(&image(), ON), Step::Image);
        assert_eq!(decide(&location(), ON), Step::Location);
        assert_eq!(decide(&Input::Unsupported, ON), Step::Unsupported);
        assert_eq!(decide(&audio(), ON), Step::Unsupported);
        assert_eq!(decide(&audio(), State { voice: true, ..ON }), Step::Voice);
    }

    #[test]
    fn help_actions_are_open_before_onboarding() {
        for action in [
            "help",
            "help.privacy",
            "help.report",
            "onboarding.accept",
            "tab",
        ] {
            assert!(open_before_onboarding(action), "{action}");
        }
        for action in [
            "helpful",
            "helper.x",
            "buy",
            "my_orders",
            "onboarding",
            "confirm",
            "report",
            "delete_account",
        ] {
            assert!(!open_before_onboarding(action), "{action}");
        }
    }
}
