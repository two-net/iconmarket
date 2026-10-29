//! Data retention (SRS §5.4, NFR-PRIV-06): the daily job that keeps the log-type tables
//! bounded, as no table is partitioned (DR-08). It deletes what has been kept longer than its
//! `retention.*Days`:
//!
//! | Data | Kept for | Counted from |
//! | --- | --- | --- |
//! | Webhook inbox events, once processed or given up | `webhookEventDays` | processing |
//! | AI conversation memory | `aiMessageDays` | the turn |
//! | AI call logs | `aiCallLogDays` | the call |
//! | Push logs, once sent or given up | `pushLogDays` | the last try |
//! | Payment-slip images | `slipDays` | the order's final status |
//! | PromptPay QR codes (FR-PAY-04) | until the order is final | — |
//! | Images of deleted products and shops | `deletedImageDays` | the deletion |
//! | Audit log | `auditLogDays` | the action |
//!
//! Orders, their items and events are kept for good and only anonymized (DR-04). Sessions and
//! pending actions are deleted as they expire, by the scheduler's `delete-expired` job.
//!
//! Each rule is one statement, committed on its own, that deletes only what is past its cutoff:
//! a run cut short or run again deletes nothing it shouldn't.

#[cfg(test)]
mod tests;

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter,
    QuerySelect, QueryTrait, RelationTrait,
};

use crate::config::RetentionConfig;
use crate::persistence::entity::image::OwnerType;
use crate::persistence::entity::{
    ai_call_log, ai_message, audit_log, image, order, payment_slip, product, push_log, shop,
    webhook_event,
};

/// What [`apply`] deleted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub webhook_events: u64,
    pub ai_messages: u64,
    pub ai_call_logs: u64,
    pub push_logs: u64,
    /// Images of payment slips; the slips themselves are kept, without an image
    pub slip_images: u64,
    /// PromptPay QR codes of orders that are final
    pub payment_qr_images: u64,
    /// Images of deleted products and shops
    pub deleted_owner_images: u64,
    pub audit_logs: u64,
}

/// Deletes what, at `now`, has been kept longer than `config` allows (§5.4). A row exactly at
/// its limit is kept.
pub async fn apply(
    db: &impl ConnectionTrait,
    config: &RetentionConfig,
    now: DateTimeUtc,
) -> Result<Deleted, DbErr> {
    let before = |days: u32| now - TimeDelta::days(days.into());

    // Events still waiting in the inbox stay, however old
    let webhook_events = webhook_event::Entity::delete_many()
        .filter(
            webhook_event::Column::Status
                .is_in([webhook_event::Status::Done, webhook_event::Status::Failed]),
        )
        .filter(webhook_event::Column::ProcessedAt.lt(before(config.webhook_event_days)))
        .exec(db)
        .await?
        .rows_affected;
    let ai_messages = ai_message::Entity::delete_many()
        .filter(ai_message::Column::CreatedAt.lt(before(config.ai_message_days)))
        .exec(db)
        .await?
        .rows_affected;
    let ai_call_logs = ai_call_log::Entity::delete_many()
        .filter(ai_call_log::Column::CreatedAt.lt(before(config.ai_call_log_days)))
        .exec(db)
        .await?
        .rows_affected;
    // A pending push is the retry job's to send or give up
    let push_logs = push_log::Entity::delete_many()
        .filter(push_log::Column::Status.is_in([push_log::Status::Sent, push_log::Status::Failed]))
        .filter(push_log::Column::SentAt.lt(before(config.push_log_days)))
        .exec(db)
        .await?
        .rows_affected;
    let slip_images = slip_images(db, before(config.slip_days)).await?;
    let payment_qr_images = payment_qr_images(db, now).await?;
    let deleted_owner_images = deleted_owner_images(db, before(config.deleted_image_days)).await?;
    let audit_logs = audit_log::Entity::delete_many()
        .filter(audit_log::Column::CreatedAt.lt(before(config.audit_log_days)))
        .exec(db)
        .await?
        .rows_affected;

    Ok(Deleted {
        webhook_events,
        ai_messages,
        ai_call_logs,
        push_logs,
        slip_images,
        payment_qr_images,
        deleted_owner_images,
        audit_logs,
    })
}

/// Deletes the slip images of orders that reached a final status before `cutoff`. Their slips
/// lose the image (`ON DELETE SET NULL`), so its link finds nothing.
async fn slip_images(db: &impl ConnectionTrait, cutoff: DateTimeUtc) -> Result<u64, DbErr> {
    let finished = payment_slip::Entity::find()
        .select_only()
        .column(payment_slip::Column::Id)
        .join(JoinType::InnerJoin, payment_slip::Relation::Order.def())
        .filter(final_before(cutoff))
        .into_query();
    let deleted = image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(OwnerType::Slip))
        .filter(image::Column::OwnerId.in_subquery(finished))
        .exec(db)
        .await?;
    Ok(deleted.rows_affected)
}

/// Deletes the PromptPay QR codes of orders that reached a final status before `cutoff`: only
/// their buyer needed them, to pay, and they show the shop's PromptPay ID.
async fn payment_qr_images(db: &impl ConnectionTrait, cutoff: DateTimeUtc) -> Result<u64, DbErr> {
    let finished = order::Entity::find()
        .select_only()
        .column(order::Column::Id)
        .filter(final_before(cutoff))
        .into_query();
    let deleted = image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(OwnerType::PaymentQr))
        .filter(image::Column::OwnerId.in_subquery(finished))
        .exec(db)
        .await?;
    Ok(deleted.rows_affected)
}

/// Orders that reached a final status (Table 4.6-A) before `cutoff`, each by the time it
/// reached that status.
fn final_before(cutoff: DateTimeUtc) -> Condition {
    let finals = [
        (order::Status::Completed, order::Column::CompletedAt),
        (order::Status::Rejected, order::Column::RejectedAt),
        (order::Status::Cancelled, order::Column::CancelledAt),
        (order::Status::Expired, order::Column::ExpiredAt),
    ];
    let mut any = Condition::any();
    for (status, at) in finals {
        any = any.add(
            Condition::all()
                .add(order::Column::Status.eq(status))
                .add(at.lt(cutoff)),
        );
    }
    any
}

/// Deletes the images of products and shops deleted before `cutoff`.
async fn deleted_owner_images(
    db: &impl ConnectionTrait,
    cutoff: DateTimeUtc,
) -> Result<u64, DbErr> {
    let products = product::Entity::find()
        .select_only()
        .column(product::Column::Id)
        .filter(product::Column::DeletedAt.lt(cutoff))
        .into_query();
    let shops = shop::Entity::find()
        .select_only()
        .column(shop::Column::Id)
        .filter(shop::Column::DeletedAt.lt(cutoff))
        .into_query();
    let owned_by = |owner_type: OwnerType, owners| {
        Condition::all()
            .add(image::Column::OwnerType.eq(owner_type))
            .add(image::Column::OwnerId.in_subquery(owners))
    };
    let deleted = image::Entity::delete_many()
        .filter(
            Condition::any()
                .add(owned_by(OwnerType::Product, products))
                .add(owned_by(OwnerType::Shop, shops)),
        )
        .exec(db)
        .await?;
    Ok(deleted.rows_affected)
}
