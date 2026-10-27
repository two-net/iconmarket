//! Problem reports (FR-HELP-02): any user can report a product or shop, with a reason. The report
//! enters the admins' review queue ("รายการรอตรวจ", #31), and the admins are alerted when
//! `notification.adminAlerts` is on (Table 4.8-A).
//!
//! A report starts from the "แจ้งปัญหา" [`button`] of a product or shop card (#24, #28), which
//! carries the ID of what it reports. The reason is picked from common ones or typed in a
//! [flow](ReportFlow) of one step, and the report is filed once it is given. A user has at most
//! one open report on the same product or shop, so tapping again doesn't alert the admins again.
//!
//! Reporting needs onboarding complete (FR-UI-12), and suspended users can't (FR-ADM-04). The
//! alert doesn't name the reporter; the review queue does.

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use super::access::{self, Denied, authorize};
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::{self, Item};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message};
use crate::notifications::templates::{Reported, labels};
use crate::notifications::{Kind, To};
use crate::persistence::entity::{product, report, shop};
use crate::thai::Label;

/// The action of the "แจ้งปัญหา" button, which names a product in `p` or a shop in `s`
pub const REPORT: &str = "report";
const PRODUCT: &str = "p";
const SHOP: &str = "s";

/// The longest reason, in characters
const REASON_CHARS: usize = 300;

/// The common reasons offered, sent as if typed (FR-UI-02)
static REASONS: [&Label; 4] = [
    &labels::REASON_WRONG_INFO,
    &labels::REASON_PROHIBITED,
    &labels::REASON_SCAM,
    &labels::REASON_RUDE,
];

/// What a report is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Product(i64),
    Shop(i64),
}

impl Target {
    /// What the button that sent `postback` reports.
    fn of(postback: &Postback) -> Option<Self> {
        match (postback.int(PRODUCT), postback.int(SHOP)) {
            (Some(id), None) => Some(Self::Product(id)),
            (None, Some(id)) => Some(Self::Shop(id)),
            _ => None,
        }
    }

    fn postback(self) -> Postback {
        let report = Postback::new(REPORT);
        match self {
            Self::Product(id) => report.with_int(PRODUCT, id),
            Self::Shop(id) => report.with_int(SHOP, id),
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

/// The "แจ้งปัญหา" button of a product or shop card (#24, #28).
pub fn button(target: Target) -> Action {
    target.postback().button(labels::REPORT.clone())
}

/// The flow asking why the user reports `target`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportFlow {
    target: Target,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Reason,
}

impl Flow for ReportFlow {
    const NAME: &'static str = "report";
    type Step = Step;
}

/// A tap on "แจ้งปัญหา": asks why, unless the user can't report what it names.
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    let Some(target) = Target::of(postback) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let found = match check(turn, target).await? {
        Ok(found) => found,
        Err(refused) => return Ok(vec![refused]),
    };
    let session = Session {
        step: Step::Reason,
        draft: ReportFlow { target },
    };
    flow::start(turn, session, vec![ask(turn, &found)]).await
}

/// The reason, picked or typed (FR-UI-12 step 3): files the report and alerts the admins.
/// `None` for anything but text, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    session: Session<ReportFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let templates = turn.templates;
    let target = session.draft.target;
    let done = |messages| async move {
        flow::answered(turn, Answer::<ReportFlow>::Done(messages))
            .await
            .map(Some)
    };
    // The user can be suspended, and what they report deleted, mid-flow
    let found = match check(turn, target).await? {
        Ok(found) => found,
        Err(refused) => return done(vec![refused]).await,
    };
    let reason = match reason(text) {
        Ok(reason) => reason,
        Err(refused) => {
            let again = match refused {
                Refused::Empty => ask(turn, &found),
                Refused::TooLong(max) => templates.text_too_long(max),
            };
            let again = Answer::<ReportFlow>::Again(vec![again]);
            return flow::answered(turn, again).await.map(Some);
        }
    };

    let (target_type, target_id) = target.stored();
    let row = report::ActiveModel {
        reporter_id: Set(Some(turn.actor.user.id)),
        target_type: Set(target_type),
        target_id: Set(target_id),
        reason: Set(reason.clone()),
        status: Set(report::Status::Open),
        resolved_by_id: Set(None),
        resolved_at: Set(None),
        created_at: Set(turn.now),
        ..Default::default()
    };
    report::Entity::insert(row)
        .exec_without_returning(turn.db)
        .await?;
    let what = found.reported();
    let review = Item::ReviewQueue
        .postback()
        .button(labels::REVIEW_QUEUE.clone());
    let alert = templates.report_alert(&what, &reason, turn.now, review);
    turn.notify(Kind::UserReport, To::Admins, vec![alert]);
    done(vec![
        templates.report_received(&what),
        menu::main_menu(&turn.actor),
    ])
    .await
}

/// A product or shop as found to be reported.
struct Found {
    /// The product's name, or the shop's
    name: String,
    /// The product's shop's name; `None` for a shop
    shop: Option<String>,
    /// The owner of the shop, or of the product's shop
    owner_id: i64,
}

impl Found {
    fn reported(&self) -> Reported<'_> {
        match &self.shop {
            Some(shop) => Reported::Product {
                name: &self.name,
                shop,
            },
            None => Reported::Shop { name: &self.name },
        }
    }
}

/// What `target` names, if the user may report it now: it can be reported, it isn't theirs,
/// and they haven't reported it already. Otherwise the reply saying why not.
async fn check(turn: &Turn<'_>, target: Target) -> Result<Result<Found, Message>, DbErr> {
    let templates = turn.templates;
    let user_id = turn.actor.user.id;
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Report).await? {
        return Ok(Err(turn.denied(denied)));
    }
    let Some(found) = find(turn.db, target).await? else {
        return Ok(Err(templates.denied(Denied::NotFound)));
    };
    if found.owner_id == user_id {
        return Ok(Err(templates.denied(Denied::NotAllowed)));
    }
    if reported_by(turn.db, user_id, target).await? {
        return Ok(Err(templates.already_reported(&found.reported())));
    }
    Ok(Ok(found))
}

/// `target`, if it can be reported: a product that was listed and isn't deleted, or a shop
/// that buyers see, or did until it was suspended.
async fn find(db: &impl ConnectionTrait, target: Target) -> Result<Option<Found>, DbErr> {
    let (product, shop_id) = match target {
        Target::Product(id) => match product::Entity::find_by_id(id).one(db).await? {
            Some(product)
                if !matches!(
                    product.status,
                    product::Status::Draft | product::Status::Deleted
                ) =>
            {
                let shop_id = product.shop_id;
                (Some(product), shop_id)
            }
            _ => return Ok(None),
        },
        Target::Shop(id) => (None, id),
    };
    let Some(shop) = shop::Entity::find_by_id(shop_id).one(db).await? else {
        return Ok(None);
    };
    Ok(match product {
        Some(product) => Some(Found {
            name: product.name,
            shop: Some(shop.name),
            owner_id: shop.owner_id,
        }),
        None if matches!(
            shop.status,
            shop::Status::Approved | shop::Status::Suspended
        ) =>
        {
            Some(Found {
                name: shop.name,
                shop: None,
                owner_id: shop.owner_id,
            })
        }
        None => None,
    })
}

/// Whether user `user_id` reported `target` and admins have yet to deal with it.
async fn reported_by(
    db: &impl ConnectionTrait,
    user_id: i64,
    target: Target,
) -> Result<bool, DbErr> {
    let (target_type, target_id) = target.stored();
    let open = report::Entity::find()
        .filter(report::Column::ReporterId.eq(user_id))
        .filter(report::Column::TargetType.eq(target_type))
        .filter(report::Column::TargetId.eq(target_id))
        .filter(report::Column::Status.eq(report::Status::Open))
        .count(db)
        .await?;
    Ok(open > 0)
}

/// What asks why the user reports `found`, offering the common reasons.
fn ask(turn: &Turn<'_>, found: &Found) -> Message {
    let reasons = REASONS
        .iter()
        .map(|&label| Action::message(label.clone(), label.as_str()))
        .collect();
    turn.templates
        .ask_report_reason(&found.reported(), REASON_CHARS, reasons)
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
