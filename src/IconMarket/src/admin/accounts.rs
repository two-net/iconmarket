//! "ผู้ใช้" (FR-ADM-04): admins find a user by display name, phone or order number, see how
//! their account and shop stand, and suspend them or lift their suspension with a reason.
//!
//! The search is typed in a [flow](FindFlow) of one step. An order number finds its buyer and its
//! shop's owner; a phone, the users and the shops' owners who gave it; anything else, the users
//! whose display name contains it, whatever the case. Deleted accounts are never found. Each user
//! found has a card: their name, roles, zone and when they joined, whether they are suspended
//! and why, and their shop and how it stands, but no phone or house number (NFR-PRIV-04).
//!
//! A suspended user is told why, and while suspended can't order, sell or use the AI, but can
//! still see their orders and get help ([`authorize`]); buyers see neither their shop nor its
//! products ([`listing`](crate::catalog::browse::listing)). Every refusal repeats the reason
//! ([`Turn::denied`]). An admin can't be suspended: the role comes from `admin.lineUserIds`
//! (FR-ADM-01), which is also how an admin loses it. A shop suspended from the review queue
//! ([`review`](super::review)) has its suspension lifted from its owner's card here.
//!
//! Each change asks why in a [flow](ReasonFlow) of one step, tells the user, and is audited in
//! the transaction that makes it (FR-ADM-08). The buttons carry only the user's ID, and both the
//! tap and the reason check that the change still applies: another admin may have made it
//! already (FR-UI-13). No change here touches an order (FR-ADM-09), and none is made by the AI:
//! they are taken from these menus only (FR-AI-17).

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::extension::postgres::PgExpr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use super::audit;
use super::reason::{REASON_CHARS, Refused, reason};
use crate::catalog::browse::search;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message};
use crate::notifications::templates::{USERS_FOUND, UserCard, UserShop, labels};
use crate::notifications::{Kind, To};
use crate::persistence::entity::{order, shop, user};
use crate::thai::{self, Label};
use crate::users::access::{self, Denied, authorize};
use crate::users::roles::Roles;

/// What the actions of a card's buttons start with
pub const PREFIX: &str = "accounts.";
/// The actions of a card's buttons; the user's ID is in `u`
pub const SUSPEND: &str = "accounts.suspend";
pub const UNSUSPEND: &str = "accounts.unsuspend";
pub const UNSUSPEND_SHOP: &str = "accounts.unsuspend_shop";
const USER: &str = "u";

/// The longest search, in characters
const QUERY_CHARS: usize = 100;

/// The common reasons for suspending a user, sent as if typed (FR-UI-02)
static SUSPENSION_REASONS: [&Label; 3] = [
    &labels::REASON_SCAM,
    &labels::REASON_PROHIBITED,
    &labels::REASON_RUDE,
];

/// The common reasons for lifting a suspension
static LIFTING_REASONS: [&Label; 2] = [&labels::REASON_RESOLVED, &labels::REASON_CHECKED];

/// The flow asking who to find. It has no draft: the search is all it asks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindFlow {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindStep {
    Query,
}

impl Flow for FindFlow {
    const NAME: &'static str = "find_user";
    type Step = FindStep;
}

/// The flow asking why a user's account or shop changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonFlow {
    change: Change,
    user_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasonStep {
    Reason,
}

impl Flow for ReasonFlow {
    const NAME: &'static str = "account_reason";
    type Step = ReasonStep;
}

/// What an admin changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Change {
    /// Suspends the user's account
    Suspend,
    /// Lifts its suspension
    Unsuspend,
    /// Lifts their shop's suspension
    UnsuspendShop,
}

/// A user, and their shop if they have one.
#[derive(Clone, Debug)]
struct Account {
    user: user::Model,
    shop: Option<shop::Model>,
}

impl Account {
    /// Their card, with the buttons of the changes that apply.
    fn card(&self) -> UserCard<'_> {
        let roles = Roles::of(&self.user, self.shop.as_ref());
        UserCard {
            display_name: self.user.display_name.as_deref(),
            seller: roles.seller,
            admin: roles.admin,
            suspended: self.user.status == user::Status::Suspended,
            suspension_reason: self.user.suspension_reason.as_deref(),
            zone: self.user.zone.as_deref(),
            joined: self.user.created_at,
            shop: self.shop.as_ref().map(|shop| UserShop {
                name: &shop.name,
                status: shop.status,
                suspension_reason: shop.suspension_reason.as_deref(),
            }),
            buttons: self.buttons(),
        }
    }

    /// "ระงับบัญชี" for an active user who isn't an admin, "ยกเลิกการระงับ" for a suspended one,
    /// and "ยกเลิกระงับร้าน" for the owner of a suspended shop.
    fn buttons(&self) -> Vec<Action> {
        let acts = |action| Postback::new(action).with_int(USER, self.user.id);
        let mut buttons = Vec::new();
        match self.user.status {
            user::Status::Active if !self.user.is_admin => {
                buttons.push(acts(SUSPEND).button(labels::SUSPEND_ACCOUNT.clone()));
            }
            user::Status::Suspended => {
                buttons.push(acts(UNSUSPEND).button(labels::UNSUSPEND_ACCOUNT.clone()));
            }
            user::Status::Active | user::Status::Deleted => {}
        }
        if self.shop_suspended() {
            buttons.push(acts(UNSUSPEND_SHOP).button(labels::UNSUSPEND_SHOP.clone()));
        }
        buttons
    }

    fn shop_suspended(&self) -> bool {
        self.shop
            .as_ref()
            .is_some_and(|shop| shop.status == shop::Status::Suspended)
    }
}

/// A tap on "ผู้ใช้", or on a button of a user's card.
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let change = match postback.action() {
        action if action == Item::Users.action() => {
            let session = Session {
                step: FindStep::Query,
                draft: FindFlow::default(),
            };
            return flow::start(turn, session, vec![turn.templates.ask_user_query()]).await;
        }
        SUSPEND => Change::Suspend,
        UNSUSPEND => Change::Unsuspend,
        UNSUSPEND_SHOP => Change::UnsuspendShop,
        _ => return Ok(vec![turn.templates.denied(Denied::NotFound)]),
    };
    let Some(user_id) = postback.int(USER) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let account = match current(turn, user_id, change).await? {
        Ok(account) => account,
        Err(messages) => return Ok(messages),
    };
    let session = Session {
        step: ReasonStep::Reason,
        draft: ReasonFlow { change, user_id },
    };
    flow::start(turn, session, vec![ask(turn, change, &account)]).await
}

/// The search, typed (FR-UI-12 step 3): the users it finds, or the step again if none. `None`
/// for anything but text, so routing goes on (FR-UI-14).
pub async fn answer_query(
    turn: &Turn<'_>,
    _session: Session<FindFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let templates = turn.templates;
    let again = |messages| async move {
        flow::answered(turn, Answer::<FindFlow>::Again(messages))
            .await
            .map(Some)
    };
    let done = |messages| async move {
        flow::answered(turn, Answer::<FindFlow>::Done(messages))
            .await
            .map(Some)
    };
    // The admin role can be lost mid-flow (FR-ADM-01)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return done(vec![turn.denied(denied)]).await;
    }
    let query = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        return again(vec![templates.ask_user_query()]).await;
    }
    if query.chars().count() > QUERY_CHARS {
        return again(vec![templates.text_too_long(QUERY_CHARS)]).await;
    }
    let (accounts, more) = find(turn.db, &query).await?;
    if accounts.is_empty() {
        return again(vec![templates.no_user_found(&query)]).await;
    }
    let cards: Vec<_> = accounts.iter().map(Account::card).collect();
    done(templates.users_found(&cards, more)).await
}

/// The reason, picked or typed (FR-UI-12 step 3): makes the change and tells the user. `None`
/// for anything but text, so routing goes on (FR-UI-14).
pub async fn answer_reason(
    turn: &Turn<'_>,
    session: Session<ReasonFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let done = |messages| async move {
        flow::answered(turn, Answer::<ReasonFlow>::Done(messages))
            .await
            .map(Some)
    };
    // The admin role can be lost mid-flow (FR-ADM-01)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return done(vec![turn.denied(denied)]).await;
    }
    let ReasonFlow { change, user_id } = session.draft;
    // Another admin may have made the change meanwhile
    let account = match current(turn, user_id, change).await? {
        Ok(account) => account,
        Err(current) => return done(current).await,
    };
    let reason = match reason(text) {
        Ok(reason) => reason,
        Err(refused) => {
            let again = match refused {
                Refused::Empty => ask(turn, change, &account),
                Refused::TooLong(max) => turn.templates.text_too_long(max),
            };
            let again = Answer::<ReasonFlow>::Again(vec![again]);
            return flow::answered(turn, again).await.map(Some);
        }
    };
    let reply = match change {
        Change::Suspend => suspend(turn, &account, &reason).await?,
        Change::Unsuspend => unsuspend(turn, &account, &reason).await?,
        Change::UnsuspendShop => unsuspend_shop(turn, &account, &reason).await?,
    };
    done(vec![reply]).await
}

/// Suspends the user for `reason`, which they are told.
async fn suspend(turn: &Turn<'_>, account: &Account, reason: &str) -> Result<Message, DbErr> {
    let user = &account.user;
    set_status(turn, user.id, user::Status::Suspended, Some(reason)).await?;
    let entry = entry(turn, audit::Action::UserSuspended, user.id, reason);
    audit::record(turn.db, turn.now, [entry]).await?;
    let templates = turn.templates;
    let suspended = templates.account_suspended_notice(reason);
    turn.notify(Kind::AccountSuspended, To::User(user.id), vec![suspended]);
    info!(user_id = user.id, "user suspended");
    Ok(templates.user_suspended(user.display_name.as_deref()))
}

/// Lifts the user's suspension, for `reason`, and tells them.
async fn unsuspend(turn: &Turn<'_>, account: &Account, reason: &str) -> Result<Message, DbErr> {
    let user = &account.user;
    set_status(turn, user.id, user::Status::Active, None).await?;
    let entry = entry(turn, audit::Action::UserUnsuspended, user.id, reason);
    audit::record(turn.db, turn.now, [entry]).await?;
    let templates = turn.templates;
    let unsuspended = templates.account_unsuspended_notice();
    turn.notify(
        Kind::AccountUnsuspended,
        To::User(user.id),
        vec![unsuspended],
    );
    info!(user_id = user.id, "user unsuspended");
    Ok(templates.user_unsuspended(user.display_name.as_deref()))
}

/// Lifts the suspension of the user's shop, for `reason`, and tells them: they are a seller
/// again (FR-UI-10).
async fn unsuspend_shop(
    turn: &Turn<'_>,
    account: &Account,
    reason: &str,
) -> Result<Message, DbErr> {
    let shop = account.shop.as_ref().expect("a suspended shop");
    shop::Entity::update_many()
        .set(shop::ActiveModel {
            status: Set(shop::Status::Approved),
            suspension_reason: Set(None),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(shop::Column::Id.eq(shop.id))
        .exec(turn.db)
        .await?;
    let entry = audit::Entry {
        actor_id: Some(turn.actor.user.id),
        action: audit::Action::ShopUnsuspended,
        target: audit::Target::Shop(shop.id),
        reason: Some(reason.to_owned()),
        details: audit::details([("name", json!(shop.name))]),
    };
    audit::record(turn.db, turn.now, [entry]).await?;
    let templates = turn.templates;
    let my_shop = Item::MyShop.postback().button(labels::MY_SHOP.clone());
    let unsuspended = templates.shop_unsuspended_notice(&shop.name, my_shop);
    turn.notify(
        Kind::ShopUnsuspended,
        To::User(shop.owner_id),
        vec![unsuspended],
    );
    turn.roles_changed(shop.owner_id);
    info!(shop_id = shop.id, "shop unsuspended");
    Ok(templates.shop_unsuspended(&shop.name))
}

/// Makes user `id` `status`, suspended for `reason`, or not.
async fn set_status(
    turn: &Turn<'_>,
    id: i64,
    status: user::Status,
    reason: Option<&str>,
) -> Result<(), DbErr> {
    user::Entity::update_many()
        .set(user::ActiveModel {
            status: Set(status),
            suspension_reason: Set(reason.map(str::to_owned)),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(user::Column::Id.eq(id))
        .exec(turn.db)
        .await?;
    Ok(())
}

/// The audit entry of `action`, done by the admin acting to user `user_id`, for `reason`. It
/// keeps no display name, which is personal data (NFR-PRIV-05).
fn entry(turn: &Turn<'_>, action: audit::Action, user_id: i64, reason: &str) -> audit::Entry {
    audit::Entry {
        actor_id: Some(turn.actor.user.id),
        action,
        target: audit::Target::User(user_id),
        reason: Some(reason.to_owned()),
        details: audit::details([]),
    }
}

/// User `user_id`, locked until the event's transaction ends, if `change` still applies to
/// them; otherwise what to show instead: why not, and their card as it is now (FR-UI-13).
async fn current(
    turn: &Turn<'_>,
    user_id: i64,
    change: Change,
) -> Result<Result<Account, Vec<Message>>, DbErr> {
    let templates = turn.templates;
    let found = user::Entity::find_by_id(user_id)
        .filter(user::Column::Status.ne(user::Status::Deleted))
        .lock_exclusive()
        .one(turn.db)
        .await?;
    let Some(user) = found else {
        return Ok(Err(vec![templates.denied(Denied::NotFound)]));
    };
    let shop = shop::Entity::find()
        .filter(shop::Column::OwnerId.eq(user.id))
        .lock_exclusive()
        .one(turn.db)
        .await?;
    let account = Account { user, shop };
    let name = account.user.display_name.as_deref();
    let suspended = account.user.status == user::Status::Suspended;
    let refused = match change {
        Change::Suspend if account.user.is_admin => Some(templates.admin_not_suspended()),
        Change::Suspend if suspended => Some(templates.account_already(name, true)),
        Change::Unsuspend if !suspended => Some(templates.account_already(name, false)),
        Change::UnsuspendShop if !account.shop_suspended() => Some(templates.shop_not_suspended()),
        Change::Suspend | Change::Unsuspend | Change::UnsuspendShop => None,
    };
    Ok(match refused {
        Some(refused) => Err(vec![refused, templates.user_card(&account.card())]),
        None => Ok(account),
    })
}

/// What asks why `change` is made to `account`, offering the common reasons.
fn ask(turn: &Turn<'_>, change: Change, account: &Account) -> Message {
    let offered = |reasons: &[&Label]| {
        reasons
            .iter()
            .map(|&label| Action::message(label.clone(), label.as_str()))
            .collect()
    };
    let templates = turn.templates;
    let name = account.user.display_name.as_deref();
    match (change, &account.shop) {
        (Change::Suspend, _) => {
            templates.ask_suspension_reason(name, REASON_CHARS, offered(&SUSPENSION_REASONS))
        }
        (Change::UnsuspendShop, Some(shop)) => templates.ask_shop_unsuspension_reason(
            &shop.name,
            REASON_CHARS,
            offered(&LIFTING_REASONS),
        ),
        (Change::Unsuspend | Change::UnsuspendShop, _) => {
            templates.ask_unsuspension_reason(name, REASON_CHARS, offered(&LIFTING_REASONS))
        }
    }
}

/// The users `query` finds, in order, at most [`USERS_FOUND`], and whether more matched: by
/// order number, by phone, or by display name.
async fn find(db: &impl ConnectionTrait, query: &str) -> Result<(Vec<Account>, bool), DbErr> {
    // An order number, as on its card: its buyer, and its shop's owner
    let number = query.replace(' ', "").to_uppercase();
    let found = order::Entity::find()
        .filter(order::Column::OrderNumber.eq(number))
        .one(db)
        .await?;
    if let Some(order) = found {
        let owner: Option<i64> = shop::Entity::find_by_id(order.shop_id)
            .select_only()
            .column(shop::Column::OwnerId)
            .into_tuple()
            .one(db)
            .await?;
        let ids: Vec<i64> = std::iter::once(order.buyer_id).chain(owner).collect();
        return Ok((accounts(db, &ids).await?, false));
    }
    let mut ids: Vec<i64> = match thai::parse_phone(query) {
        // A phone: the users who gave it, then the shops' owners who did
        Some(phone) => {
            let users: Vec<i64> = user::Entity::find()
                .select_only()
                .column(user::Column::Id)
                .filter(user::Column::Phone.eq(phone.as_str()))
                .order_by_asc(user::Column::Id)
                .into_tuple()
                .all(db)
                .await?;
            let owners: Vec<i64> = shop::Entity::find()
                .select_only()
                .column(shop::Column::OwnerId)
                .filter(shop::Column::Phone.eq(phone.as_str()))
                .order_by_asc(shop::Column::OwnerId)
                .into_tuple()
                .all(db)
                .await?;
            users.into_iter().chain(owners).collect()
        }
        // A display name, or part of one, whatever the case
        None => {
            let pattern = format!("%{}%", search::escaped(query));
            user::Entity::find()
                .select_only()
                .column(user::Column::Id)
                .filter(Expr::col((user::Entity, user::Column::DisplayName)).ilike(pattern))
                .filter(user::Column::Status.ne(user::Status::Deleted))
                .order_by_asc(user::Column::DisplayName)
                .order_by_asc(user::Column::Id)
                .limit(u64::try_from(USERS_FOUND + 1).expect("a few rows"))
                .into_tuple()
                .all(db)
                .await?
        }
    };
    let mut seen = HashSet::new();
    ids.retain(|&id| seen.insert(id));
    let mut accounts = accounts(db, &ids).await?;
    let more = accounts.len() > USERS_FOUND;
    accounts.truncate(USERS_FOUND);
    Ok((accounts, more))
}

/// Users `ids` that aren't deleted, in that order, with their shops.
async fn accounts(db: &impl ConnectionTrait, ids: &[i64]) -> Result<Vec<Account>, DbErr> {
    let users = user::Entity::find()
        .filter(user::Column::Id.is_in(ids.iter().copied()))
        .filter(user::Column::Status.ne(user::Status::Deleted))
        .all(db)
        .await?;
    let mut shops = shop::Entity::find()
        .filter(shop::Column::OwnerId.is_in(users.iter().map(|user| user.id)))
        .all(db)
        .await?;
    let mut accounts: Vec<Account> = users
        .into_iter()
        .map(|user| {
            let shop = shops
                .iter()
                .position(|shop| shop.owner_id == user.id)
                .map(|at| shops.swap_remove(at));
            Account { user, shop }
        })
        .collect();
    accounts.sort_by_key(|account| ids.iter().position(|&id| id == account.user.id));
    Ok(accounts)
}
