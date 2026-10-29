//! The daily sales summary (FR-NOT-06), while `notification.dailySummary` is on: at
//! `notification.dailySummaryTime` each seller is pushed how their shop's day went, the orders
//! placed today and the sales completed today, whenever they were placed, so that a pre-order
//! counts on the day it is handed over (FR-PROD-11). While `ai.features.assistant` is on, a short
//! comment the AI writes on the figures goes with them.
//!
//! Pushes count against the OA's monthly quota (NFR-COST-01), so a seller is pushed only on a day
//! their shop had an order placed or a sale completed, and only if a push reaches them (FR-NOT-02):
//! the owner of an `APPROVED` or `HELD` shop, who may see the "ยอดขาย" its button opens, with an
//! active account. The AI is asked about those shops only.
//!
//! Each shop's summary is queued in a transaction of its own that marks its day as summarized
//! (`shops.summary_sent_on`), and sent once that commits: a run cut short and run again pushes no
//! seller twice a day (FR-JOB-03). A run overdue past midnight, the app having been down at the
//! time, sends nothing: the day before is not made up for, nor anyone pushed at night.
//!
//! The call is the system's, so it counts against the global daily limit only (FR-AI-09). The
//! figures are the system's own; the product names are the seller's, given as data (FR-AI-06).
//! Without an answer, or with one that is no short line, the summary goes without a comment
//! (FR-AI-10, NFR-REL-06). What the AI wrote is never logged.

#[cfg(test)]
mod tests;

use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, JoinType,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, RelationTrait,
    TransactionTrait,
};
use serde_json::json;
use tracing::{debug, info};

use super::sales::{self, TopProduct};
use crate::ai::{self, Call, DATA_RULE, Feature, Gateway, Output};
use crate::config::NotificationConfig;
use crate::dispatcher::menu::Item;
use crate::line::{self, Action};
use crate::notifications::templates::{Templates, labels};
use crate::notifications::{self, Kind, Outbox, To};
use crate::orders::periods::since;
use crate::persistence::entity::{order, shop, user};
use crate::thai::Money;

/// What the AI is told of its task; the particle to end with and [`DATA_RULE`] follow.
const RULES: &str = "\
You write the closing comment of a seller's daily sales summary for IconMarket: a LINE \
marketplace where neighbours in one Thai village sell to each other. Their shop's figures for \
today follow, with yesterday's sales to compare. Write one or two short, warm sentences in Thai \
that encourage the seller, such as on what sold best or how today compares. Use no figure that \
is not given, and make up nothing. No greeting, emoji, list or formatting.";

/// The longest comment shown, in characters; a longer one is no short comment
pub const COMMENT_CHARS: usize = 200;

/// A shop's business day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Day {
    /// The orders placed on it, whatever their status now
    pub placed: u64,
    /// The orders completed on it, whenever they were placed
    pub completed: u64,
    /// Their total
    pub completed_value: Money,
    /// The products that sold the most in them, by value, at most [`sales::TOP`]
    pub top: Vec<TopProduct>,
}

/// Pushes each seller whose shop had an order placed or a sale completed on the day of `now`
/// their summary of it, if `notification.dailySummary` is on, unless they had it already.
/// Returns how many were queued.
pub async fn send(
    db: &DatabaseConnection,
    line: &line::Client,
    ai: &Gateway,
    templates: &Templates,
    notification: &NotificationConfig,
    now: DateTime<Utc>,
) -> Result<u64, DbErr> {
    if !notification.daily_summary {
        return Ok(0);
    }
    let time_zone = templates.time_zone();
    let local = now.with_timezone(&time_zone);
    if local.time() < notification.daily_summary_time {
        info!("the daily summary is overdue from a day before, so it is not sent");
        return Ok(0);
    }
    let today = local.date_naive();
    let from = since(now, time_zone, 1);
    let yesterday = since(now, time_zone, 2);

    let mut sent = 0;
    for shop in to_summarize(db, today, from).await? {
        let day = gather(db, shop.id, from).await?;
        let (before, before_value) = completed(db, shop.id, yesterday, Some(from)).await?;
        let comment = comment(ai, templates, &day, before, before_value).await;
        let notice = templates.daily_summary_notice(now, &day, comment.as_deref(), buttons());

        let txn = db.begin().await?;
        // Another run may have summarized it meanwhile
        let marked = shop::Entity::update_many()
            .col_expr(shop::Column::SummarySentOn, Expr::value(today))
            .filter(shop::Column::Id.eq(shop.id))
            .filter(not_summarized(today))
            .exec(&txn)
            .await?
            .rows_affected;
        if marked == 0 {
            txn.rollback().await?;
            continue;
        }
        let mut outbox = Outbox::default();
        outbox.add(Kind::DailySummary, To::User(shop.owner_id), vec![notice]);
        let queued = outbox.queue(&txn, notification, now).await?;
        txn.commit().await?;
        if !queued.is_empty() {
            sent += 1;
        }
        notifications::send(db, line, queued).await;
    }
    Ok(sent)
}

/// The summary's buttons: to see the shop's sales (FR-SHOP-11) and its orders.
fn buttons() -> Vec<Action> {
    vec![
        Item::Sales.postback().button(labels::SALES.clone()),
        Item::ShopOrders
            .postback()
            .button(labels::SHOP_ORDERS.clone()),
    ]
}

/// The shops to summarize `today`, which began at `from`, in ID order: those whose owner may see
/// their sales and can be pushed to, that had an order placed or completed today, and whose
/// summary of today isn't queued yet.
async fn to_summarize(
    db: &impl ConnectionTrait,
    today: NaiveDate,
    from: DateTime<Utc>,
) -> Result<Vec<shop::Model>, DbErr> {
    let busy = order::Entity::find()
        .select_only()
        .column(order::Column::ShopId)
        .filter(
            Condition::any()
                .add(order::Column::CreatedAt.gte(from))
                .add(order::Column::CompletedAt.gte(from)),
        )
        .into_query();
    shop::Entity::find()
        .join(JoinType::InnerJoin, shop::Relation::Owner.def())
        .filter(shop::Column::Status.is_in([shop::Status::Approved, shop::Status::Held]))
        .filter(not_summarized(today))
        .filter(shop::Column::Id.in_subquery(busy))
        // As `notifications::reaches` judges them, so that the AI isn't asked about the others
        .filter(user::Column::Status.eq(user::Status::Active))
        .filter(user::Column::IsFollowing.eq(true))
        .filter(user::Column::LineUserId.is_not_null())
        .order_by_asc(shop::Column::Id)
        .all(db)
        .await
}

/// Shops whose summary of `today` isn't queued yet.
fn not_summarized(today: NaiveDate) -> Condition {
    Condition::any()
        .add(shop::Column::SummarySentOn.is_null())
        .add(shop::Column::SummarySentOn.lt(today))
}

/// Shop `shop_id`'s day that began at `from`.
async fn gather(
    db: &impl ConnectionTrait,
    shop_id: i64,
    from: DateTime<Utc>,
) -> Result<Day, DbErr> {
    let placed = order::Entity::find()
        .filter(order::Column::ShopId.eq(shop_id))
        .filter(order::Column::CreatedAt.gte(from))
        .count(db)
        .await?;
    let (completed, completed_value) = completed(db, shop_id, from, None).await?;
    let top = sales::top(db, shop_id, order::Column::CompletedAt.gte(from)).await?;
    Ok(Day {
        placed,
        completed,
        completed_value,
        top,
    })
}

/// How many of shop `shop_id`'s orders were completed from `from` on, and before `until` if
/// given, and their total.
async fn completed(
    db: &impl ConnectionTrait,
    shop_id: i64,
    from: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
) -> Result<(u64, Money), DbErr> {
    let mut select = order::Entity::find()
        .select_only()
        .column_as(order::Column::Id.count(), "orders")
        .column_as(order::Column::Total.sum(), "value")
        .filter(order::Column::ShopId.eq(shop_id))
        .filter(order::Column::Status.eq(order::Status::Completed))
        .filter(order::Column::CompletedAt.gte(from));
    if let Some(until) = until {
        select = select.filter(order::Column::CompletedAt.lt(until));
    }
    let found: Option<(i64, Option<Money>)> = select.into_tuple().one(db).await?;
    let (count, value) = found.unwrap_or_default();
    let count = count.try_into().expect("a count is never negative");
    Ok((count, value.unwrap_or(Money::ZERO)))
}

/// The AI's comment on `day`, compared with the `before` orders completed the day before for
/// `before_value`, while `ai.features.assistant` is on: one line of at most [`COMMENT_CHARS`].
async fn comment(
    ai: &Gateway,
    templates: &Templates,
    day: &Day,
    before: u64,
    before_value: Money,
) -> Option<String> {
    if !ai.is_enabled(Feature::Assistant) {
        return None;
    }
    let best_sellers: Vec<_> = day
        .top
        .iter()
        .map(|product| {
            json!({
                "name": product.name,
                "quantity": product.quantity,
                "unit": product.unit,
                "value": product.value.to_string(),
            })
        })
        .collect();
    let figures = json!({
        "today": {
            "orders_placed": day.placed,
            "sales_completed": day.completed,
            "sales_value": day.completed_value.to_string(),
            "best_sellers": best_sellers,
        },
        "yesterday": {
            "sales_completed": before,
            "sales_value": before_value.to_string(),
        },
    });
    let call = Call {
        feature: Feature::Assistant,
        // The system's call, not the seller's (FR-AI-09)
        user_id: None,
        system: format!(
            "{RULES} End sentences with {}.\n\n{DATA_RULE}",
            templates.particle()
        ),
        messages: vec![ai::Message::user(ai::as_data("sales", &figures))],
        output: Output::Text,
    };
    let comment = match ai.complete(call).await {
        Ok(ai::Answer::Text(text)) => one_line(&text),
        // Never: what asks for text is answered with text (FR-AI-05)
        Ok(_) => None,
        Err(unavailable) => {
            debug!(?unavailable, "no AI comment on the daily summary");
            return None;
        }
    };
    debug!(
        commented = comment.is_some(),
        "AI comment on the daily summary"
    );
    comment
}

/// `text` on one line, if it is a short comment: something, of at most [`COMMENT_CHARS`].
fn one_line(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty() && line.chars().count() <= COMMENT_CHARS).then_some(line)
}
