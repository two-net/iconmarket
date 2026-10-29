//! The village group digest (FR-NOT-07), while `notification.groupDigest` is on: at
//! `notification.groupDigestTime` "ของใหม่วันนี้" is posted to the village LINE group,
//! `line.villageGroupId`: the first page of the very list buyers see under that name
//! ([`Listing::NewToday`]), so the group is shown the same products in the same order.
//!
//! A card's buttons would send postbacks from the group, which the dispatcher ignores there
//! (FR-LINE-06), and answering one in the group would show everyone what one member did. So each
//! card, and its "ดูเพิ่มเติม", opens the chat with the OA instead, where buyers browse and
//! order as usual. The link needs the OA's basic ID, which LINE is asked for on each run.
//!
//! A push to a group counts once per member against the OA's monthly quota (NFR-COST-01), so the
//! digest is posted only on a day with something new, and only while the OA is in the group: its
//! `join` and `leave` events are kept in `line_groups`. A group the OA was never seen leaving is
//! taken to have it, as it may have joined before the digest was switched on.
//!
//! The digest is queued in a transaction that marks the group's day as posted
//! (`line_groups.digest_sent_on`), and sent once that commits: a run cut short and run again
//! posts no second digest that day (FR-JOB-03). A run overdue past midnight, the app having been
//! down at the time, posts nothing: the day before is not made up for.

#[cfg(test)]
mod tests;

use anyhow::Context;
use chrono::{DateTime, NaiveDate, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, SimpleExpr};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, TransactionTrait};
use tracing::{debug, info};
use url::Url;

use super::listing::{self, Listing};
use super::{card, card_photos};
use crate::config::NotificationConfig;
use crate::line::{self, Action, Message};
use crate::notifications::templates::{ProductCard, Templates, labels};
use crate::notifications::{self, Kind};
use crate::persistence::entity::line_group;

/// Where a link to a chat with an OA starts; the OA's basic ID follows
const CHAT: &str = "https://line.me/R/oaMessage";

/// Posts "ของใหม่วันนี้" to the village group `group_id` at `now`, if `notification.groupDigest`
/// is on, the OA is in the group, something is new and it wasn't posted today already. Returns
/// whether it was queued. Fails, posting nothing, if LINE won't give the OA's basic ID.
pub async fn post(
    db: &DatabaseConnection,
    line: &line::Client,
    templates: &Templates,
    notification: &NotificationConfig,
    group_id: Option<&str>,
    base: &Url,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let Some(group_id) = group_id.filter(|_| notification.group_digest) else {
        return Ok(false);
    };
    let local = now.with_timezone(&templates.time_zone());
    if local.time() < notification.group_digest_time {
        info!("the village group digest is overdue from a day before, so it is not posted");
        return Ok(false);
    }
    let today = local.date_naive();
    if let Some(group) = line_group::Entity::find_by_id(group_id).one(db).await? {
        if group.left_at.is_some() {
            info!("the OA is not in the village group, so the digest is not posted");
            return Ok(false);
        }
        if group.digest_sent_on == Some(today) {
            debug!("the village group digest was posted today already");
            return Ok(false);
        }
    }
    let page = listing::page(db, Listing::NewToday, now, 0).await?;
    if page.items.is_empty() {
        debug!("nothing new today, so no village group digest is posted");
        return Ok(false);
    }

    let info = line
        .bot_info()
        .await
        .context("cannot get the OA's basic ID for the village group digest")?;
    let chat = chat_link(&info.basic_id);
    let button = Action::Uri {
        label: labels::ORDER_IN_CHAT.clone(),
        uri: chat.to_string(),
    };
    let ids: Vec<i64> = page.items.iter().map(|(product, _)| product.id).collect();
    let photos = card_photos(db, base, &ids).await?;
    let cards: Vec<ProductCard<'_>> = page
        .items
        .iter()
        .zip(&photos)
        .map(|((product, shop), photo)| ProductCard {
            add_to_cart: None,
            button: button.clone(),
            details: button.clone(),
            ..card(product, shop, photo, today)
        })
        .collect();
    let messages = templates.group_digest(now, &cards, &chat, page.next.is_some());

    let txn = db.begin().await?;
    // Another run may have posted it meanwhile
    let marked = line_group::Entity::insert(line_group::ActiveModel {
        group_id: Set(group_id.to_owned()),
        digest_sent_on: Set(Some(today)),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(line_group::Column::GroupId)
            .update_column(line_group::Column::DigestSentOn)
            .action_and_where(not_posted(today))
            .to_owned(),
    )
    .exec_without_returning(&txn)
    .await?;
    if marked == 0 {
        txn.rollback().await?;
        return Ok(false);
    }
    let queued = notifications::queue_group(
        &txn,
        notification,
        Kind::GroupDigest,
        group_id,
        &messages,
        now,
    )
    .await?;
    txn.commit().await?;
    let posted = !queued.is_empty();
    notifications::send(db, line, queued).await;
    Ok(posted)
}

/// The OA was added to group `group_id` at `now`, while the digest is on: it is in the group from
/// then on. The village group, `village_group`, is told what the OA posts there, and when.
pub async fn joined(
    db: &impl ConnectionTrait,
    templates: &Templates,
    notification: &NotificationConfig,
    village_group: Option<&str>,
    group_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Message>, DbErr> {
    let row = line_group::ActiveModel {
        group_id: Set(group_id.to_owned()),
        joined_at: Set(Some(now)),
        left_at: Set(None),
        ..Default::default()
    };
    line_group::Entity::insert(row)
        .on_conflict(
            OnConflict::column(line_group::Column::GroupId)
                .update_columns([line_group::Column::JoinedAt, line_group::Column::LeftAt])
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    let village = village_group == Some(group_id);
    info!(village, "the OA was added to a group");
    Ok(if village {
        vec![templates.group_joined(notification.group_digest_time)]
    } else {
        Vec::new()
    })
}

/// The OA was removed from group `group_id` at `now`, while the digest is on: nothing is posted
/// there until it is added again.
pub async fn left(
    db: &impl ConnectionTrait,
    group_id: &str,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    let row = line_group::ActiveModel {
        group_id: Set(group_id.to_owned()),
        left_at: Set(Some(now)),
        ..Default::default()
    };
    line_group::Entity::insert(row)
        .on_conflict(
            OnConflict::column(line_group::Column::GroupId)
                .update_column(line_group::Column::LeftAt)
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    info!("the OA was removed from a group");
    Ok(())
}

/// Groups whose digest of `today` isn't queued yet, as the stored row says.
fn not_posted(today: NaiveDate) -> SimpleExpr {
    let sent_on = Expr::col((line_group::Entity, line_group::Column::DigestSentOn));
    sent_on.clone().is_null().or(sent_on.lt(today))
}

/// The link that opens the chat with the OA of basic ID `basic_id`, such as `@216ruvtu`.
fn chat_link(basic_id: &str) -> Url {
    let mut url = Url::parse(CHAT).expect("a valid URL");
    url.path_segments_mut()
        .expect("an https URL")
        .push(basic_id)
        .push("");
    url
}
