//! "รายการรอตรวจ" (FR-ADM-03): what moderation held for review and what users reported, for the
//! admins to decide on. Both are open [reports](report): moderation files one of no one's for each
//! product or shop it holds (FR-PROD-09), and users one each with their reason (FR-HELP-02). The
//! queue shows a card for each product or shop with open reports, the one waiting longest first,
//! with its state and its latest reports, who made them and when.
//!
//! An admin approves what was held, which buyers then see again: a product is listed, and a shop
//! approved. They remove a product, held or reported, or suspend its shop, or a shop reported:
//! both ask why in a [flow](ReasonFlow) of one step, and its owner is told the reason (Table
//! 4.8-A). A removed product is never listed again, and buyers see neither a suspended shop nor
//! its products (FR-SHOP-10) until its suspension is lifted from its owner's card in "ผู้ใช้"
//! ([`accounts`](super::accounts)). Or they dismiss the reports, leaving it as it is. Each
//! decision closes the reports its card showed, and is audited in the transaction that makes it
//! (FR-ADM-08); a report filed since keeps the product or shop in the queue.
//!
//! Each card's buttons carry what it shows, by ID: the product or shop, its version, its
//! `updated_at`, which editing it and deciding change, and the latest report shown. A tap on a
//! card that no longer matches, as another admin decided or its seller edited it since, shows it
//! as it is now instead of acting (FR-UI-13). No decision changes an order (FR-ADM-09), and none
//! is made by the AI: they are taken from these menus only (FR-AI-17).

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;
use url::Url;

use super::audit;
use super::reason::{REASON_CHARS, Refused, reason};
use crate::catalog::images;
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message};
use crate::notifications::templates::{
    REVIEWS_PER_PAGE, Reported, Reporter, ReviewCard, ReviewReport, Reviewed, Templates, labels,
};
use crate::notifications::{Kind, To};
use crate::persistence::entity::{image, product, report, shop, user};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// What the actions of a card's buttons start with
pub const PREFIX: &str = "review.";
/// The actions of a card's buttons: its product's ID is in `p`, or its shop's in `s`, the
/// version it showed in `v`, and the latest report it showed in `r`
pub const APPROVE: &str = "review.approve";
pub const REMOVE: &str = "review.remove";
pub const SUSPEND: &str = "review.suspend";
pub const DISMISS: &str = "review.dismiss";
const PRODUCT: &str = "p";
const SHOP: &str = "s";
const VERSION: &str = "v";
const LATEST: &str = "r";
/// The page "ดูเพิ่มเติม" shows, on the postback of "รายการรอตรวจ"
const PAGE: &str = "page";

/// The common reasons offered, sent as if typed (FR-UI-02)
static REASONS: [&Label; 4] = [
    &labels::REASON_PROHIBITED,
    &labels::REASON_WRONG_INFO,
    &labels::REASON_SCAM,
    &labels::REASON_RUDE,
];

/// What a card is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    Product(i64),
    Shop(i64),
}

impl Target {
    /// What the button that sent `postback` acts on.
    fn of(postback: &Postback) -> Option<Self> {
        match (postback.int(PRODUCT), postback.int(SHOP)) {
            (Some(id), None) => Some(Self::Product(id)),
            (None, Some(id)) => Some(Self::Shop(id)),
            _ => None,
        }
    }

    /// The postback of `action` on it.
    fn postback(self, action: &'static str) -> Postback {
        let postback = Postback::new(action);
        match self {
            Self::Product(id) => postback.with_int(PRODUCT, id),
            Self::Shop(id) => postback.with_int(SHOP, id),
        }
    }

    /// As `reports` keeps it.
    fn stored(self) -> (report::TargetType, i64) {
        match self {
            Self::Product(id) => (report::TargetType::Product, id),
            Self::Shop(id) => (report::TargetType::Shop, id),
        }
    }
}

/// What a card showed, which its buttons act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    target: Target,
    /// The version of the product or shop: its `updated_at` in microseconds
    version: i64,
    /// The ID of the latest report shown
    latest: i64,
}

impl Seen {
    fn of(postback: &Postback) -> Option<Self> {
        Some(Self {
            target: Target::of(postback)?,
            version: postback.int(VERSION)?,
            latest: postback.int(LATEST)?,
        })
    }
}

/// The flow asking why a product is removed or a shop suspended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonFlow {
    act: Act,
    seen: Seen,
}

/// What the reason is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Act {
    /// Removing the product
    Remove,
    /// Suspending the shop, or the product's
    Suspend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Reason,
}

impl Flow for ReasonFlow {
    const NAME: &'static str = "review_reason";
    type Step = Step;
}

/// A product with its shop, or a shop, as it is now.
#[derive(Clone, Debug)]
struct Found {
    target: Target,
    product: Option<product::Model>,
    /// The shop, or the product's
    shop: shop::Model,
}

impl Found {
    /// Its version, which its card's buttons carry: its `updated_at` in microseconds, as
    /// PostgreSQL keeps it.
    fn version(&self) -> i64 {
        let updated_at = match &self.product {
            Some(product) => product.updated_at,
            None => self.shop.updated_at,
        };
        updated_at.timestamp_micros()
    }

    fn reported(&self) -> Reported<'_> {
        match &self.product {
            Some(product) => Reported::Product {
                name: &product.name,
                shop: &self.shop.name,
            },
            None => Reported::Shop {
                name: &self.shop.name,
            },
        }
    }

    fn state(&self) -> Reviewed {
        match &self.product {
            Some(product) => Reviewed::Product {
                status: product.status,
                shop: self.shop.status,
            },
            None => Reviewed::Shop(self.shop.status),
        }
    }

    /// Whether it is held for review, so it can be approved; it is then not dismissed.
    fn may_approve(&self) -> bool {
        match &self.product {
            Some(product) => product.status == product::Status::Held,
            None => self.shop.status == shop::Status::Held,
        }
    }

    /// Whether it is a product that can still be removed: held, listed or hidden.
    fn may_remove(&self) -> bool {
        self.product.as_ref().is_some_and(|product| {
            matches!(
                product.status,
                product::Status::Held | product::Status::Active | product::Status::Hidden
            )
        })
    }

    /// Whether its shop, or the product's, can still be suspended: it is run by its owner.
    fn may_suspend(&self) -> bool {
        matches!(
            self.shop.status,
            shop::Status::Approved | shop::Status::Held
        )
    }

    /// Whether it can still be done to it.
    fn allows(&self, act: Act) -> bool {
        match act {
            Act::Remove => self.may_remove(),
            Act::Suspend => self.may_suspend(),
        }
    }

    /// Its card's buttons, for the reports up to `latest`: approve, remove and suspend while
    /// they apply, and dismiss unless it is held for review.
    fn buttons(&self, latest: i64) -> Vec<Action> {
        let acts = |action| {
            self.target
                .postback(action)
                .with_int(VERSION, self.version())
                .with_int(LATEST, latest)
        };
        let mut buttons = Vec::new();
        if self.may_approve() {
            buttons.push(acts(APPROVE).button(labels::APPROVE.clone()));
        }
        if self.may_remove() {
            buttons.push(acts(REMOVE).button(labels::REMOVE_PRODUCT.clone()));
        }
        if self.may_suspend() {
            buttons.push(acts(SUSPEND).button(labels::SUSPEND_SHOP.clone()));
        }
        if !self.may_approve() {
            buttons.push(acts(DISMISS).button(labels::DISMISS.clone()));
        }
        buttons
    }

    /// The audit entry of `action`, done by the admin acting, for `reason`, closing `reports`.
    fn entry(
        &self,
        turn: &Turn<'_>,
        action: audit::Action,
        reason: Option<&str>,
        reports: &[i64],
    ) -> audit::Entry {
        // Suspending a product's shop, or approving a shop, acts on the shop
        let on_shop = matches!(
            action,
            audit::Action::ShopApproved | audit::Action::ShopSuspended
        );
        let (target, details) = match &self.product {
            Some(product) if !on_shop => (
                audit::Target::Product(product.id),
                audit::details([
                    ("name", json!(product.name)),
                    ("shop", json!(self.shop.name)),
                    ("reports", json!(reports)),
                ]),
            ),
            _ => (
                audit::Target::Shop(self.shop.id),
                audit::details([("name", json!(self.shop.name)), ("reports", json!(reports))]),
            ),
        };
        audit::Entry {
            actor_id: Some(turn.actor.user.id),
            action,
            target,
            reason: reason.map(str::to_owned),
            details,
        }
    }
}

/// A tap on "รายการรอตรวจ", or on one of its cards' buttons.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let action = postback.action();
    if action == Item::ReviewQueue.action() {
        let page = postback
            .int(PAGE)
            .and_then(|page| usize::try_from(page).ok())
            .unwrap_or(0);
        return Ok(vec![list(turn.db, turn.templates, config, page).await?]);
    }
    let act = match action {
        REMOVE => Some(Act::Remove),
        SUSPEND => Some(Act::Suspend),
        APPROVE | DISMISS => None,
        _ => return Ok(vec![turn.templates.denied(Denied::NotFound)]),
    };
    let Some(seen) = Seen::of(postback) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let found = match current(turn, config, seen).await? {
        Ok(found) => found,
        Err(messages) => return Ok(messages),
    };
    match (action, act) {
        (APPROVE, _) if found.may_approve() => approve(turn, &found, seen).await,
        (DISMISS, _) if !found.may_approve() => dismiss(turn, &found, seen).await,
        (_, Some(act)) if found.allows(act) => {
            let session = Session {
                step: Step::Reason,
                draft: ReasonFlow { act, seen },
            };
            flow::start(turn, session, vec![ask(turn, act, &found)]).await
        }
        // Not offered for its state as it is now, which a version of a product doesn't cover:
        // its shop's state
        _ => changed(turn, config, &found).await,
    }
}

/// The reason, picked or typed (FR-UI-12 step 3): removes the product or suspends the shop, and
/// tells its owner why. `None` for anything but text, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
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
    let ReasonFlow { act, seen } = session.draft;
    // Another admin may have decided meanwhile, or the seller changed it
    let found = match current(turn, config, seen).await? {
        Ok(found) if found.allows(act) => found,
        Ok(found) => return done(changed(turn, config, &found).await?).await,
        Err(current) => return done(current).await,
    };
    let reason = match reason(text) {
        Ok(reason) => reason,
        Err(refused) => {
            let again = match refused {
                Refused::Empty => ask(turn, act, &found),
                Refused::TooLong(max) => turn.templates.text_too_long(max),
            };
            let again = Answer::<ReasonFlow>::Again(vec![again]);
            return flow::answered(turn, again).await.map(Some);
        }
    };
    let reply = match act {
        Act::Remove => remove(turn, &found, seen, &reason).await?,
        Act::Suspend => suspend(turn, &found, seen, &reason).await?,
    };
    done(vec![reply]).await
}

/// Lists the product held for review, or approves the held shop, which buyers then see again.
async fn approve(turn: &Turn<'_>, found: &Found, seen: Seen) -> Result<Vec<Message>, DbErr> {
    let action = match &found.product {
        Some(product) => {
            set_product(turn, product.id, product::Status::Active).await?;
            audit::Action::ProductApproved
        }
        None => {
            shop::Entity::update_many()
                .set(shop::ActiveModel {
                    status: Set(shop::Status::Approved),
                    updated_at: Set(turn.now),
                    ..Default::default()
                })
                .filter(shop::Column::Id.eq(found.shop.id))
                .exec(turn.db)
                .await?;
            audit::Action::ShopApproved
        }
    };
    let reports = close(turn, seen, report::Status::Resolved).await?;
    let entry = found.entry(turn, action, None, &reports);
    audit::record(turn.db, turn.now, [entry]).await?;
    info!(target = ?found.target, "approved after review");
    Ok(vec![turn.templates.review_approved(&found.reported())])
}

/// Removes the product for `reason`, which its seller is told.
async fn remove(
    turn: &Turn<'_>,
    found: &Found,
    seen: Seen,
    reason: &str,
) -> Result<Message, DbErr> {
    let product = found.product.as_ref().expect("only a product is removed");
    set_product(turn, product.id, product::Status::Removed).await?;
    let reports = close(turn, seen, report::Status::Resolved).await?;
    let entry = found.entry(turn, audit::Action::ProductRemoved, Some(reason), &reports);
    audit::record(turn.db, turn.now, [entry]).await?;
    let templates = turn.templates;
    let my_products = Item::MyProducts
        .postback()
        .button(labels::MY_PRODUCTS.clone());
    let removed = templates.product_removed_notice(&product.name, reason, my_products);
    turn.notify(
        Kind::ItemRemoved,
        To::User(found.shop.owner_id),
        vec![removed],
    );
    info!(product_id = product.id, "product removed");
    Ok(templates.product_removed(&product.name))
}

/// Suspends the shop, or the product's, for `reason`, which its owner is told. Its owner is no
/// longer a seller while it is (FR-UI-10).
async fn suspend(
    turn: &Turn<'_>,
    found: &Found,
    seen: Seen,
    reason: &str,
) -> Result<Message, DbErr> {
    let shop = &found.shop;
    shop::Entity::update_many()
        .set(shop::ActiveModel {
            status: Set(shop::Status::Suspended),
            suspension_reason: Set(Some(reason.to_owned())),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(shop::Column::Id.eq(shop.id))
        .exec(turn.db)
        .await?;
    let reports = close(turn, seen, report::Status::Resolved).await?;
    let entry = found.entry(turn, audit::Action::ShopSuspended, Some(reason), &reports);
    audit::record(turn.db, turn.now, [entry]).await?;
    let templates = turn.templates;
    let suspended = templates.shop_suspended_notice(&shop.name, reason);
    turn.notify(
        Kind::ShopSuspended,
        To::User(shop.owner_id),
        vec![suspended],
    );
    turn.roles_changed(shop.owner_id);
    info!(shop_id = shop.id, "shop suspended");
    Ok(templates.shop_suspended(&shop.name))
}

/// Closes the reports, leaving the product or shop as it is.
async fn dismiss(turn: &Turn<'_>, found: &Found, seen: Seen) -> Result<Vec<Message>, DbErr> {
    let reports = close(turn, seen, report::Status::Dismissed).await?;
    let entry = found.entry(turn, audit::Action::ReportsDismissed, None, &reports);
    audit::record(turn.db, turn.now, [entry]).await?;
    info!(target = ?found.target, "reports dismissed");
    Ok(vec![turn.templates.reports_dismissed(&found.reported())])
}

/// Makes product `id` `status`.
async fn set_product(turn: &Turn<'_>, id: i64, status: product::Status) -> Result<(), DbErr> {
    product::Entity::update_many()
        .set(product::ActiveModel {
            status: Set(status),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(product::Column::Id.eq(id))
        .exec(turn.db)
        .await?;
    Ok(())
}

/// Makes the open reports a card showed `status`, resolved by the admin acting: their IDs.
async fn close(turn: &Turn<'_>, seen: Seen, status: report::Status) -> Result<Vec<i64>, DbErr> {
    let ids: Vec<i64> = open(seen.target)
        .select_only()
        .column(report::Column::Id)
        .filter(report::Column::Id.lte(seen.latest))
        .into_tuple()
        .all(turn.db)
        .await?;
    report::Entity::update_many()
        .set(report::ActiveModel {
            status: Set(status),
            resolved_by_id: Set(Some(turn.actor.user.id)),
            resolved_at: Set(Some(turn.now)),
            ..Default::default()
        })
        .filter(report::Column::Id.is_in(ids.clone()))
        .exec(turn.db)
        .await?;
    Ok(ids)
}

/// The open reports on `target`, oldest first.
fn open(target: Target) -> sea_orm::Select<report::Entity> {
    let (target_type, target_id) = target.stored();
    report::Entity::find()
        .filter(report::Column::Status.eq(report::Status::Open))
        .filter(report::Column::TargetType.eq(target_type))
        .filter(report::Column::TargetId.eq(target_id))
        .order_by_asc(report::Column::Id)
}

/// What `seen` names, locked until the event's transaction ends, if it is still as its card
/// showed it and its reports are still open; otherwise what to show instead: the queue, or it as
/// it is now with fresh buttons (FR-UI-13).
async fn current(
    turn: &Turn<'_>,
    config: &Config,
    seen: Seen,
) -> Result<Result<Found, Vec<Message>>, DbErr> {
    let templates = turn.templates;
    let Some(found) = find(turn.db, seen.target).await? else {
        return Ok(Err(vec![
            templates.denied(Denied::NotFound),
            list(turn.db, templates, config, 0).await?,
        ]));
    };
    let shown = open(seen.target)
        .filter(report::Column::Id.lte(seen.latest))
        .one(turn.db)
        .await?;
    if shown.is_none() {
        return Ok(Err(vec![
            templates.review_handled(),
            list(turn.db, templates, config, 0).await?,
        ]));
    }
    if found.version() != seen.version {
        return changed(turn, config, &found).await.map(Err);
    }
    Ok(Ok(found))
}

/// `target`, locked until the event's transaction ends, with its shop if a product.
async fn find(db: &impl ConnectionTrait, target: Target) -> Result<Option<Found>, DbErr> {
    let (product, shop_id) = match target {
        Target::Product(id) => {
            let found = product::Entity::find_by_id(id)
                .lock_exclusive()
                .one(db)
                .await?;
            let Some(product) = found else {
                return Ok(None);
            };
            let shop_id = product.shop_id;
            (Some(product), shop_id)
        }
        Target::Shop(id) => (None, id),
    };
    let shop = shop::Entity::find_by_id(shop_id)
        .lock_exclusive()
        .one(db)
        .await?;
    Ok(shop.map(|shop| Found {
        target,
        product,
        shop,
    }))
}

/// What a tap on a card that no longer matches gets: that it changed, and its card as it is now,
/// or the queue once it has no open reports.
async fn changed(turn: &Turn<'_>, config: &Config, found: &Found) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let entries = queue(turn.db, Some(found.target)).await?;
    let Some(entry) = entries.first() else {
        return Ok(vec![
            templates.review_handled(),
            list(turn.db, templates, config, 0).await?,
        ]);
    };
    let names = reporters(turn.db, &entries).await?;
    let photos = photos(turn.db, config, &entries).await?;
    let card = card(entry, &names, photos.get(&entry.found.target));
    Ok(vec![
        templates.review_changed(),
        templates.review_card(&card),
    ])
}

/// What asks why `act` is done to `found`, offering the common reasons.
fn ask(turn: &Turn<'_>, act: Act, found: &Found) -> Message {
    let reasons = REASONS
        .iter()
        .map(|&label| Action::message(label.clone(), label.as_str()))
        .collect();
    let templates = turn.templates;
    match (act, &found.product) {
        (Act::Remove, Some(product)) => {
            templates.ask_removal_reason(&product.name, &found.shop.name, REASON_CHARS, reasons)
        }
        _ => templates.ask_shop_suspension_reason(&found.shop.name, REASON_CHARS, reasons),
    }
}

/// A product or shop in the queue, with its open reports, oldest first.
struct Waiting {
    found: Found,
    reports: Vec<report::Model>,
}

/// The products and shops with open reports, the one whose oldest report is oldest first; or
/// `only` that one.
async fn queue(db: &impl ConnectionTrait, only: Option<Target>) -> Result<Vec<Waiting>, DbErr> {
    let mut select = report::Entity::find().filter(report::Column::Status.eq(report::Status::Open));
    if let Some(target) = only {
        let (target_type, target_id) = target.stored();
        select = select
            .filter(report::Column::TargetType.eq(target_type))
            .filter(report::Column::TargetId.eq(target_id));
    }
    let reports = select
        .order_by_asc(report::Column::CreatedAt)
        .order_by_asc(report::Column::Id)
        .all(db)
        .await?;
    let mut order: Vec<Target> = Vec::new();
    let mut grouped: HashMap<Target, Vec<report::Model>> = HashMap::new();
    for report in reports {
        let target = match report.target_type {
            report::TargetType::Product => Target::Product(report.target_id),
            report::TargetType::Shop => Target::Shop(report.target_id),
        };
        grouped
            .entry(target)
            .or_insert_with(|| {
                order.push(target);
                Vec::new()
            })
            .push(report);
    }
    let ids =
        |of: fn(Target) -> Option<i64>| order.iter().copied().filter_map(of).collect::<Vec<_>>();
    let product_ids = ids(|target| match target {
        Target::Product(id) => Some(id),
        Target::Shop(_) => None,
    });
    let shop_ids = ids(|target| match target {
        Target::Shop(id) => Some(id),
        Target::Product(_) => None,
    });
    let mut products: HashMap<i64, (product::Model, shop::Model)> = product::Entity::find()
        .join(JoinType::InnerJoin, product::Relation::Shop.def())
        .filter(product::Column::Id.is_in(product_ids))
        .select_two_required(shop::Entity)
        .all(db)
        .await?
        .into_iter()
        .map(|(product, shop)| (product.id, (product, shop)))
        .collect();
    let mut shops: HashMap<i64, shop::Model> = shop::Entity::find()
        .filter(shop::Column::Id.is_in(shop_ids))
        .all(db)
        .await?
        .into_iter()
        .map(|shop| (shop.id, shop))
        .collect();
    Ok(order
        .into_iter()
        .filter_map(|target| {
            let found = match target {
                Target::Product(id) => {
                    let (product, shop) = products.remove(&id)?;
                    Found {
                        target,
                        product: Some(product),
                        shop,
                    }
                }
                Target::Shop(id) => Found {
                    target,
                    product: None,
                    shop: shops.remove(&id)?,
                },
            };
            let reports = grouped.remove(&target)?;
            Some(Waiting { found, reports })
        })
        .collect())
}

/// The display names of those who filed `entries`' reports, by user ID.
async fn reporters(
    db: &impl ConnectionTrait,
    entries: &[Waiting],
) -> Result<HashMap<i64, Option<String>>, DbErr> {
    let ids: Vec<i64> = entries
        .iter()
        .flat_map(|entry| entry.reports.iter().filter_map(|report| report.reporter_id))
        .collect();
    let rows: Vec<(i64, Option<String>)> = user::Entity::find()
        .select_only()
        .column(user::Column::Id)
        .column(user::Column::DisplayName)
        .filter(user::Column::Id.is_in(ids))
        .into_tuple()
        .all(db)
        .await?;
    Ok(rows.into_iter().collect())
}

/// The first photo of each of `entries`' products and shops, by what it shows.
async fn photos(
    db: &impl ConnectionTrait,
    config: &Config,
    entries: &[Waiting],
) -> Result<HashMap<Target, Url>, DbErr> {
    let mut photos = HashMap::new();
    for owner_type in [image::OwnerType::Product, image::OwnerType::Shop] {
        let ids: Vec<i64> = entries
            .iter()
            .filter_map(|entry| match (entry.found.target, owner_type) {
                (Target::Product(id), image::OwnerType::Product)
                | (Target::Shop(id), image::OwnerType::Shop) => Some(id),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            continue;
        }
        let rows: Vec<(i64, Uuid)> = image::Entity::find()
            .select_only()
            .column(image::Column::OwnerId)
            .column(image::Column::Id)
            .filter(image::Column::OwnerType.eq(owner_type))
            .filter(image::Column::OwnerId.is_in(ids))
            .order_by_asc(image::Column::Position)
            .order_by_asc(image::Column::CreatedAt)
            .into_tuple()
            .all(db)
            .await?;
        for (owner_id, image_id) in rows {
            let target = match owner_type {
                image::OwnerType::Shop => Target::Shop(owner_id),
                _ => Target::Product(owner_id),
            };
            photos
                .entry(target)
                .or_insert_with(|| images::product_image_url(&config.app.base_url, image_id));
        }
    }
    Ok(photos)
}

/// `entry`'s card, with `photo`: its reports the latest first, by `names` of their reporters.
fn card<'a>(
    entry: &'a Waiting,
    names: &'a HashMap<i64, Option<String>>,
    photo: Option<&'a Url>,
) -> ReviewCard<'a> {
    let reports = entry
        .reports
        .iter()
        .rev()
        .map(|report| ReviewReport {
            by: match report.reporter_id {
                None => Reporter::Moderation,
                Some(id) => Reporter::User(names.get(&id).and_then(Option::as_deref)),
            },
            reason: &report.reason,
            at: report.created_at,
        })
        .collect();
    let latest = entry
        .reports
        .iter()
        .map(|report| report.id)
        .max()
        .unwrap_or_default();
    ReviewCard {
        what: entry.found.reported(),
        photo,
        state: entry.found.state(),
        reports,
        buttons: entry.found.buttons(latest),
    }
}

/// "รายการรอตรวจ" at page `page`: the products and shops with open reports, a card each.
async fn list(
    db: &impl ConnectionTrait,
    templates: &Templates,
    config: &Config,
    page: usize,
) -> Result<Message, DbErr> {
    let entries = queue(db, None).await?;
    let names = reporters(db, &entries).await?;
    let photos = photos(db, config, &entries).await?;
    let cards: Vec<_> = entries
        .iter()
        .map(|entry| card(entry, &names, photos.get(&entry.found.target)))
        .collect();
    let pages = cards.len().div_ceil(REVIEWS_PER_PAGE);
    let page = if page < pages { page } else { 0 };
    let more = |next| {
        let more = Item::ReviewQueue.postback().with_int(PAGE, next as i64);
        more.to_string()
    };
    Ok(templates.review_queue(&cards, page, more))
}
