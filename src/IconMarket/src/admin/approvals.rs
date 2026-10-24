//! "อนุมัติร้าน" (FR-ADM-02): admins see the shops applied for, oldest first, and approve one or
//! turn it down with a reason, which its owner is told along with the decision (FR-SHOP-04).
//! Either is audited in the transaction that makes it (FR-ADM-08). The owner of an approved
//! shop gets the seller's rich menu once the event commits (FR-UI-10).
//!
//! Each card's buttons carry the shop's ID and the version of the application it showed: its
//! `updated_at`, which applying and deciding change. A tap on a card that no longer matches,
//! because another admin decided or the owner applied again since, shows the application as
//! it is now instead of acting (FR-UI-13). The reason is typed in a [flow](RejectionFlow) of
//! one step, or picked from common ones. Shop names aren't personal data, so the audit log
//! keeps them; the reason it keeps isn't logged (NFR-PRIV-05).

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;
use url::Url;

use super::audit::{self, Entry, Target};
use crate::catalog::images::{self, Owner};
use crate::catalog::shops;
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message};
use crate::notifications::templates::{self, Applicant, Templates, labels};
use crate::notifications::{Kind, To};
use crate::persistence::entity::{image, shop, user};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// The actions of a card's buttons: its shop's ID is in `s`, and the version of the
/// application it showed in `v`
pub const APPROVE: &str = "approvals.approve";
pub const REJECT: &str = "approvals.reject";
const SHOP: &str = "s";
const VERSION: &str = "v";
/// The page "ดูเพิ่มเติม" shows, on the postback of "อนุมัติร้าน"
const PAGE: &str = "page";

/// The longest reason, in characters, as for a report (FR-HELP-02)
const REASON_CHARS: usize = 300;

/// The common reasons offered, sent as if typed (FR-UI-02)
static REASONS: [&Label; 4] = [
    &labels::REASON_INCOMPLETE,
    &labels::REASON_NOT_RESIDENT,
    &labels::REASON_PROHIBITED,
    &labels::REASON_UNREACHABLE,
];

/// The flow asking why an application is turned down.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectionFlow {
    shop_id: i64,
    /// The version of the application the admin chose to turn down
    version: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Reason,
}

impl Flow for RejectionFlow {
    const NAME: &'static str = "shop_rejection";
    type Step = Step;
}

/// The version of `shop`'s application that its buttons act on: its `updated_at` in
/// microseconds, as PostgreSQL keeps it.
pub(crate) fn version(shop: &shop::Model) -> i64 {
    shop.updated_at.timestamp_micros()
}

/// Tells the admins that `applicant` applied for `shop`, whose photo is `photo` (Table 4.8-A),
/// as in Appendix A.5: its card, with the buttons that approve it or turn it down.
pub fn alert(
    templates: &Templates,
    shop: &shop::Model,
    applicant: &user::Model,
    photo: Option<&Url>,
) -> Message {
    let applicant = Applicant {
        display_name: applicant.display_name.as_deref(),
        at: shop.updated_at,
    };
    let card = shops::card(shop, photo, Some(applicant), buttons(shop));
    templates.shop_application_alert(&card)
}

/// "อนุมัติ" and "ไม่อนุมัติ" for `shop`'s application as it is.
fn buttons(shop: &shop::Model) -> Vec<Action> {
    let acts = |action| {
        Postback::new(action)
            .with_int(SHOP, shop.id)
            .with_int(VERSION, version(shop))
    };
    vec![
        acts(APPROVE).button(labels::APPROVE.clone()),
        acts(REJECT).button(labels::REJECT.clone()),
    ]
}

/// A tap on "อนุมัติร้าน", or on one of its buttons.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let action = postback.action();
    if action == Item::ApproveShops.action() {
        let page = postback
            .int(PAGE)
            .and_then(|page| usize::try_from(page).ok())
            .unwrap_or(0);
        return Ok(vec![list(turn.db, turn.templates, config, page).await?]);
    }
    let (Some(id), Some(version)) = (postback.int(SHOP), postback.int(VERSION)) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    match action {
        APPROVE => approve(turn, config, id, version).await,
        REJECT => reject(turn, config, id, version).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// Approves shop `id` (FR-ADM-02), if its application is still the `version` the admin saw:
/// its owner is told and becomes a seller (FR-SHOP-04).
async fn approve(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    version: i64,
) -> Result<Vec<Message>, DbErr> {
    let shop = match undecided(turn, config, id, version).await? {
        Ok(shop) => shop,
        Err(current) => return Ok(current),
    };
    decide(turn, &shop, shop::Status::Approved, None).await?;
    let entry = entry(turn, audit::Action::ShopApproved, &shop, None);
    audit::record(turn.db, turn.now, [entry]).await?;
    let templates = turn.templates;
    let add_product = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    let approved = templates.shop_approved_notice(&shop.name, add_product);
    turn.notify(Kind::ShopApproved, To::User(shop.owner_id), vec![approved]);
    // The seller's menu (FR-UI-10)
    turn.roles_changed(shop.owner_id);
    info!(shop_id = shop.id, "shop approved");
    Ok(vec![templates.shop_approved(&shop.name)])
}

/// Asks why shop `id` is turned down, if its application is still the `version` the admin saw.
async fn reject(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    version: i64,
) -> Result<Vec<Message>, DbErr> {
    let shop = match undecided(turn, config, id, version).await? {
        Ok(shop) => shop,
        Err(current) => return Ok(current),
    };
    let session = Session {
        step: Step::Reason,
        draft: RejectionFlow {
            shop_id: shop.id,
            version,
        },
    };
    flow::start(turn, session, vec![ask(turn, &shop.name)]).await
}

/// The reason, picked or typed (FR-UI-12 step 3): turns the application down and tells its
/// owner why. `None` for anything but text, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    session: Session<RejectionFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let templates = turn.templates;
    let done = |messages| async move {
        flow::answered(turn, Answer::<RejectionFlow>::Done(messages))
            .await
            .map(Some)
    };
    // The admin role can be lost mid-flow (FR-ADM-01)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return done(vec![templates.denied(denied)]).await;
    }
    let RejectionFlow { shop_id, version } = session.draft;
    // Another admin may have decided meanwhile
    let shop = match undecided(turn, config, shop_id, version).await? {
        Ok(shop) => shop,
        Err(current) => return done(current).await,
    };
    let reason = match reason(text) {
        Ok(reason) => reason,
        Err(refused) => {
            let again = match refused {
                Refused::Empty => ask(turn, &shop.name),
                Refused::TooLong(max) => templates.text_too_long(max),
            };
            let again = Answer::<RejectionFlow>::Again(vec![again]);
            return flow::answered(turn, again).await.map(Some);
        }
    };

    decide(turn, &shop, shop::Status::Rejected, Some(&reason)).await?;
    let entry = entry(turn, audit::Action::ShopRejected, &shop, Some(&reason));
    audit::record(turn.db, turn.now, [entry]).await?;
    let again = Postback::new(shops::AGAIN).button(labels::APPLY_AGAIN.clone());
    let rejected = templates.shop_rejected_notice(&shop.name, &reason, again);
    turn.notify(Kind::ShopRejected, To::User(shop.owner_id), vec![rejected]);
    info!(shop_id = shop.id, "shop turned down");
    done(vec![templates.shop_rejected(&shop.name)]).await
}

/// Shop `id`, locked until the event's transaction ends, if its application awaits an admin
/// as version `version` showed it; otherwise what to show instead: how it was decided, or the
/// application as it is now with fresh buttons (FR-UI-13).
async fn undecided(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    version: i64,
) -> Result<Result<shop::Model, Vec<Message>>, DbErr> {
    let templates = turn.templates;
    let found = shop::Entity::find_by_id(id)
        .lock_exclusive()
        .one(turn.db)
        .await?;
    let Some(shop) = found else {
        return not_found(turn, config).await.map(Err);
    };
    Ok(match shop.status {
        shop::Status::Pending if self::version(&shop) == version => Ok(shop),
        // The owner applied again since the card was sent
        shop::Status::Pending => {
            let applicant = user::Entity::find_by_id(shop.owner_id).one(turn.db).await?;
            let photo = photo_url(config, images::first(turn.db, Owner::Shop(shop.id)).await?);
            let card = match &applicant {
                Some(applicant) => alert(templates, &shop, applicant, photo.as_ref()),
                None => return not_found(turn, config).await.map(Err),
            };
            Err(vec![templates.application_changed(), card])
        }
        shop::Status::Approved | shop::Status::Suspended => {
            Err(vec![templates.application_decided(&shop.name, true)])
        }
        shop::Status::Rejected => Err(vec![templates.application_decided(&shop.name, false)]),
        shop::Status::Closed => return not_found(turn, config).await.map(Err),
    })
}

/// Makes `shop` `status`, for `reason` if turned down.
async fn decide(
    turn: &Turn<'_>,
    shop: &shop::Model,
    status: shop::Status,
    reason: Option<&str>,
) -> Result<(), DbErr> {
    shop::Entity::update_many()
        .set(shop::ActiveModel {
            status: Set(status),
            rejection_reason: Set(reason.map(str::to_owned)),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(shop::Column::Id.eq(shop.id))
        .exec(turn.db)
        .await?;
    Ok(())
}

/// What asks why the shop called `name` is turned down, offering the common reasons.
fn ask(turn: &Turn<'_>, name: &str) -> Message {
    let reasons = REASONS
        .iter()
        .map(|&label| Action::message(label.clone(), label.as_str()))
        .collect();
    turn.templates
        .ask_rejection_reason(name, REASON_CHARS, reasons)
}

/// Why a reason wasn't taken; the step asks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// Nothing but spaces
    Empty,
    /// Longer than [`REASON_CHARS`], in characters
    TooLong(usize),
}

/// `text` as a reason: without spaces around it, and each run of spaces or lines within it
/// made one space.
fn reason(text: &str) -> Result<String, Refused> {
    let reason = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match reason.chars().count() {
        0 => Err(Refused::Empty),
        chars if chars > REASON_CHARS => Err(Refused::TooLong(REASON_CHARS)),
        _ => Ok(reason),
    }
}

/// "อนุมัติร้าน" at page `page`: the applications awaiting an admin, oldest first, each with
/// who applied, when, and its photo.
async fn list(
    db: &impl ConnectionTrait,
    templates: &Templates,
    config: &Config,
    page: usize,
) -> Result<Message, DbErr> {
    let pending = shop::Entity::find()
        .filter(shop::Column::Status.eq(shop::Status::Pending))
        .order_by_asc(shop::Column::UpdatedAt)
        .order_by_asc(shop::Column::Id)
        .all(db)
        .await?;
    let ids: Vec<i64> = pending.iter().map(|shop| shop.id).collect();
    let owners: HashMap<i64, user::Model> = user::Entity::find()
        .filter(user::Column::Id.is_in(pending.iter().map(|shop| shop.owner_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|user| (user.id, user))
        .collect();
    // Each shop's first photo
    let mut photos: HashMap<i64, Url> = HashMap::new();
    let rows: Vec<(i64, Uuid)> = image::Entity::find()
        .select_only()
        .column(image::Column::OwnerId)
        .column(image::Column::Id)
        .filter(image::Column::OwnerType.eq(image::OwnerType::Shop))
        .filter(image::Column::OwnerId.is_in(ids))
        .order_by_asc(image::Column::Position)
        .order_by_asc(image::Column::CreatedAt)
        .into_tuple()
        .all(db)
        .await?;
    for (shop_id, image_id) in rows {
        photos
            .entry(shop_id)
            .or_insert_with(|| images::product_image_url(&config.app.base_url, image_id));
    }

    let pages = pending.len().div_ceil(templates::APPLICATIONS_PER_PAGE);
    let page = if page < pages { page } else { 0 };
    let cards: Vec<_> = pending
        .iter()
        .map(|shop| {
            let applicant = Applicant {
                display_name: owners
                    .get(&shop.owner_id)
                    .and_then(|owner| owner.display_name.as_deref()),
                at: shop.updated_at,
            };
            shops::card(shop, photos.get(&shop.id), Some(applicant), buttons(shop))
        })
        .collect();
    let more = |next| {
        let more = Item::ApproveShops.postback().with_int(PAGE, next as i64);
        more.to_string()
    };
    Ok(templates.applications(&cards, page, more))
}

/// What a tap on an application that is gone gets: the list as it is now.
async fn not_found(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    Ok(vec![
        turn.templates.denied(Denied::NotFound),
        list(turn.db, turn.templates, config, 0).await?,
    ])
}

/// The `/img/p` link of photo `id`, if there is one.
fn photo_url(config: &Config, id: Option<Uuid>) -> Option<Url> {
    id.map(|id| images::product_image_url(&config.app.base_url, id))
}

/// The audit entry of `action`, done by the admin acting to `shop`, for `reason`.
fn entry(
    turn: &Turn<'_>,
    action: audit::Action,
    shop: &shop::Model,
    reason: Option<&str>,
) -> Entry {
    Entry {
        actor_id: Some(turn.actor.user.id),
        action,
        target: Target::Shop(shop.id),
        reason: reason.map(str::to_owned),
        details: audit::details([("name", json!(shop.name))]),
    }
}
