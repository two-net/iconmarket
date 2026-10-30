//! Retention against a migrated database (§5.4): each rule deletes what is past its limit and
//! nothing else, and a second run in a row deletes nothing more. Each test starts a throwaway
//! PostgreSQL, so they need Docker.

use std::collections::BTreeSet;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{ActiveModelTrait, DatabaseConnection, PaginatorTrait, QueryOrder};
use serde_json::json;

use super::*;
use crate::persistence::entity::{announcement, order_event, order_item, session};
use crate::persistence::testing::{
    self, Market, at, market, new_event, new_order, new_product, new_shop, new_user,
};
use crate::persistence::{orm, random_id};
use crate::thai::Money;

/// The defaults of §5.4.
fn config() -> RetentionConfig {
    RetentionConfig {
        webhook_event_days: 14,
        ai_message_days: 30,
        ai_call_log_days: 90,
        push_log_days: 90,
        slip_days: 90,
        deleted_image_days: 30,
        audit_log_days: 365,
    }
}

fn now() -> DateTimeUtc {
    at()
}

/// `days` before now: at a limit of `days`, so kept.
fn days_ago(days: i64) -> DateTimeUtc {
    now() - TimeDelta::days(days)
}

/// A microsecond past a limit of `days`.
fn past(days: i64) -> DateTimeUtc {
    days_ago(days) - TimeDelta::microseconds(1)
}

/// Applies retention at `now`, then again: the second run finds nothing more to delete.
async fn apply_twice(db: &DatabaseConnection) -> Deleted {
    let deleted = apply(db, &config(), now()).await.unwrap();
    let again = apply(db, &config(), now()).await.unwrap();
    assert_eq!(again, Deleted::default(), "a second run in a row");
    deleted
}

/// The IDs left in `E`'s table.
async fn ids<E: EntityTrait>(db: &DatabaseConnection, id: E::Column) -> Vec<i64> {
    E::find()
        .select_only()
        .column(id)
        .order_by_asc(id)
        .into_tuple()
        .all(db)
        .await
        .unwrap()
}

/// The images left.
async fn images(db: &DatabaseConnection) -> BTreeSet<Uuid> {
    let ids: Vec<Uuid> = image::Entity::find()
        .select_only()
        .column(image::Column::Id)
        .into_tuple()
        .all(db)
        .await
        .unwrap();
    ids.into_iter().collect()
}

/// A stored image of `owner_type` `owner_id`.
async fn image(db: &DatabaseConnection, owner_type: OwnerType, owner_id: i64) -> Uuid {
    image::ActiveModel {
        id: Set(random_id()),
        owner_type: Set(owner_type),
        owner_id: Set(owner_id),
        position: Set(0),
        content_type: Set("image/jpeg".to_owned()),
        width: Set(1),
        height: Set(1),
        size_bytes: Set(3),
        data: Set(vec![0xff, 0xd8, 0xff]),
        created_at: Set(past(400)),
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn log_rows_past_their_limit_are_deleted() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;

    // Inbox events go 14 days after they were processed or given up; waiting ones stay
    let mut kept_events = Vec::new();
    for (n, (status, processed_at, kept)) in [
        (webhook_event::Status::Done, Some(past(14)), false),
        (webhook_event::Status::Failed, Some(past(14)), false),
        (webhook_event::Status::Done, Some(days_ago(14)), true),
        (webhook_event::Status::Pending, None, true),
        (webhook_event::Status::Processing, None, true),
    ]
    .into_iter()
    .enumerate()
    {
        let payload = json!({ "webhookEventId": format!("01K-{n}"), "type": "message" });
        let mut row = new_event("U-buyer", payload, past(100));
        row.status = Set(status);
        row.attempts = Set(1);
        row.processed_at = Set(processed_at);
        let row = row.insert(&db).await.unwrap();
        if kept {
            kept_events.push(row.id);
        }
    }

    let mut kept_messages = Vec::new();
    for (created_at, kept) in [(past(30), false), (days_ago(30), true)] {
        let row = ai_message::ActiveModel {
            user_id: Set(market.buyer.id),
            role: Set(ai_message::Role::User),
            content: Set("ไข่ไก่ยังมีไหม".to_owned()),
            created_at: Set(created_at),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        if kept {
            kept_messages.push(row.id);
        }
    }

    let mut kept_calls = Vec::new();
    for (created_at, kept) in [(past(90), false), (days_ago(90), true)] {
        let row = ai_call_log::ActiveModel {
            user_id: Set(None),
            feature: Set("assistant".to_owned()),
            provider: Set("mock".to_owned()),
            model: Set("mock".to_owned()),
            input_tokens: Set(120),
            output_tokens: Set(40),
            latency_ms: Set(300),
            outcome: Set("ok".to_owned()),
            created_at: Set(created_at),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        if kept {
            kept_calls.push(row.id);
        }
    }

    // Pushes go 90 days after their last try; one still to send stays
    let mut kept_pushes = Vec::new();
    for (status, sent_at, kept) in [
        (push_log::Status::Sent, past(90), false),
        (push_log::Status::Failed, past(90), false),
        (push_log::Status::Sent, days_ago(90), true),
        (push_log::Status::Pending, past(90), true),
    ] {
        let row = push_log::ActiveModel {
            push_type: Set("order.new".to_owned()),
            recipient: Set("U-seller".to_owned()),
            retry_key: Set(Some(random_id())),
            status: Set(status),
            sent_at: Set(sent_at),
            messages: Set(None),
            attempts: Set(1),
            queued_at: Set(sent_at),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        if kept {
            kept_pushes.push(row.id);
        }
    }

    let mut kept_audits = Vec::new();
    for (created_at, kept) in [(past(365), false), (days_ago(365), true)] {
        let row = audit_log::ActiveModel {
            actor_id: Set(None),
            action: Set("admin.granted".to_owned()),
            target_type: Set("user".to_owned()),
            target_id: Set(Some(market.buyer.id)),
            details: Set(json!({})),
            created_at: Set(created_at),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        if kept {
            kept_audits.push(row.id);
        }
    }

    // Orders are kept for good, and sessions are the delete-expired job's
    let mut old_order = new_order(
        market.buyer.id,
        market.shop.id,
        "IM-20250101-0001",
        random_id(),
    );
    old_order.created_at = Set(past(400));
    let old_order = old_order.insert(&db).await.unwrap();
    order_item::ActiveModel {
        order_id: Set(old_order.id),
        product_id: Set(market.product.id),
        name: Set(market.product.name.clone()),
        unit: Set(market.product.unit.clone()),
        unit_price: Set(market.product.price),
        quantity: Set(2),
        line_total: Set(Money::from_baht(220)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    order_event::ActiveModel {
        order_id: Set(old_order.id),
        from_status: Set(None),
        to_status: Set(order::Status::Pending),
        actor_type: Set(order_event::ActorType::Buyer),
        actor_id: Set(Some(market.buyer.id)),
        reason: Set(None),
        created_at: Set(past(400)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    session::ActiveModel {
        user_id: Set(market.buyer.id),
        flow: Set("checkout".to_owned()),
        step: Set("confirm".to_owned()),
        data: Set(json!({})),
        expires_at: Set(past(400)),
    }
    .insert(&db)
    .await
    .unwrap();

    let deleted = apply_twice(&db).await;
    assert_eq!(
        deleted,
        Deleted {
            webhook_events: 2,
            ai_messages: 1,
            ai_call_logs: 1,
            push_logs: 2,
            slip_images: 0,
            payment_qr_images: 0,
            deleted_owner_images: 0,
            audit_logs: 1,
            announcements: 0,
        }
    );
    assert_eq!(
        ids::<webhook_event::Entity>(&db, webhook_event::Column::Id).await,
        kept_events
    );
    assert_eq!(
        ids::<ai_message::Entity>(&db, ai_message::Column::Id).await,
        kept_messages
    );
    assert_eq!(
        ids::<ai_call_log::Entity>(&db, ai_call_log::Column::Id).await,
        kept_calls
    );
    assert_eq!(
        ids::<push_log::Entity>(&db, push_log::Column::Id).await,
        kept_pushes
    );
    assert_eq!(
        ids::<audit_log::Entity>(&db, audit_log::Column::Id).await,
        kept_audits
    );
    for (table, left) in [
        ("orders", order::Entity::find().count(&db).await),
        ("order_items", order_item::Entity::find().count(&db).await),
        ("order_events", order_event::Entity::find().count(&db).await),
        ("sessions", session::Entity::find().count(&db).await),
    ] {
        assert_eq!(left.unwrap(), 1, "{table}");
    }
}

/// An order of `market`'s that reached `status` at `reached`, an hour after it was placed.
async fn order(
    db: &DatabaseConnection,
    market: &Market,
    number: &str,
    status: order::Status,
    reached: DateTimeUtc,
) -> order::Model {
    let mut row = new_order(market.buyer.id, market.shop.id, number, random_id());
    row.status = Set(status);
    row.payment_method = Set(order::PaymentMethod::PromptPay);
    row.created_at = Set(reached - TimeDelta::hours(1));
    row.updated_at = Set(reached);
    let stamp = Set(Some(reached));
    match status {
        order::Status::Accepted => row.accepted_at = stamp,
        order::Status::Completed => row.completed_at = stamp,
        order::Status::Rejected => row.rejected_at = stamp,
        order::Status::Cancelled => row.cancelled_at = stamp,
        order::Status::Expired => row.expired_at = stamp,
        other => panic!("no order reaches {other:?} in these tests"),
    }
    row.insert(db).await.unwrap()
}

/// A payment slip for `order`, with its image.
async fn slip(db: &DatabaseConnection, order: &order::Model) -> payment_slip::Model {
    let slip = payment_slip::ActiveModel {
        order_id: Set(order.id),
        access_token: Set(random_id()),
        token_expires_at: Set(order.created_at + TimeDelta::hours(24)),
        status: Set(payment_slip::Status::Submitted),
        created_at: Set(order.created_at),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let image_id = image(db, OwnerType::Slip, slip.id).await;
    let mut slip: payment_slip::ActiveModel = slip.into();
    slip.image_id = Set(Some(image_id));
    slip.update(db).await.unwrap()
}

#[tokio::test]
async fn slip_images_are_deleted_after_their_order_has_been_final_for_slip_days() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;
    let product_image = image(&db, OwnerType::Product, market.product.id).await;

    // Each final status counts from its own time
    let mut finished = Vec::new();
    for (n, status) in [
        order::Status::Completed,
        order::Status::Rejected,
        order::Status::Cancelled,
        order::Status::Expired,
    ]
    .into_iter()
    .enumerate()
    {
        let number = format!("IM-20260723-{:04}", n + 1);
        let order = order(&db, &market, &number, status, past(90)).await;
        finished.push(slip(&db, &order).await);
    }
    // Placed more than 90 days ago, but final for exactly 90
    let completed = order(
        &db,
        &market,
        "IM-20260724-0001",
        order::Status::Completed,
        days_ago(90),
    )
    .await;
    let recent = slip(&db, &completed).await;
    // Still open, however old
    let accepted = order(
        &db,
        &market,
        "IM-20260601-0001",
        order::Status::Accepted,
        past(400),
    )
    .await;
    let open = slip(&db, &accepted).await;
    // IDs are per table: the first slip shares its ID with the product, whose image stays
    assert_eq!(finished[0].id, market.product.id);

    let deleted = apply_twice(&db).await;
    assert_eq!(
        deleted,
        Deleted {
            slip_images: 4,
            ..Default::default()
        }
    );
    assert_eq!(
        images(&db).await,
        BTreeSet::from([
            product_image,
            recent.image_id.unwrap(),
            open.image_id.unwrap(),
        ])
    );
    // The slips themselves are kept, without an image
    let slips = payment_slip::Entity::find()
        .order_by_asc(payment_slip::Column::Id)
        .all(&db)
        .await
        .unwrap();
    let image_ids: Vec<_> = slips.iter().map(|slip| slip.image_id).collect();
    assert_eq!(
        image_ids,
        [None, None, None, None, recent.image_id, open.image_id]
    );
    assert_eq!(order::Entity::find().count(&db).await.unwrap(), 6);
}

/// FR-PAY-04: an order's PromptPay QR code goes as soon as the order is final, as only its buyer
/// needed it, to pay; an open order's stays, however old.
#[tokio::test]
async fn payment_qr_codes_are_deleted_once_their_order_is_final() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;
    let product_image = image(&db, OwnerType::Product, market.product.id).await;

    // Final a moment ago, whatever the status
    let mut finished = Vec::new();
    for (n, status) in [
        order::Status::Completed,
        order::Status::Rejected,
        order::Status::Cancelled,
        order::Status::Expired,
    ]
    .into_iter()
    .enumerate()
    {
        let number = format!("IM-20261024-{:04}", n + 1);
        let order = order(&db, &market, &number, status, past(0)).await;
        image(&db, OwnerType::PaymentQr, order.id).await;
        finished.push(order.id);
    }
    // Still open, however old
    let accepted = order(
        &db,
        &market,
        "IM-20260601-0001",
        order::Status::Accepted,
        past(400),
    )
    .await;
    let open = image(&db, OwnerType::PaymentQr, accepted.id).await;
    // IDs are per table: the first order shares its ID with the product, whose image stays
    assert_eq!(finished[0], market.product.id);

    let deleted = apply_twice(&db).await;
    assert_eq!(
        deleted,
        Deleted {
            payment_qr_images: 4,
            ..Default::default()
        }
    );
    assert_eq!(images(&db).await, BTreeSet::from([product_image, open]));
}

#[tokio::test]
async fn images_of_deleted_products_and_shops_are_deleted_after_deleted_image_days() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;

    let mut shops = Vec::new();
    for (owner, deleted_at) in [("U-closed", past(30)), ("U-closed-later", days_ago(30))] {
        let owner = new_user(owner).insert(&db).await.unwrap();
        let mut shop = new_shop(owner.id);
        shop.status = Set(shop::Status::Closed);
        shop.deleted_at = Set(Some(deleted_at));
        shops.push(shop.insert(&db).await.unwrap());
    }
    let [closed, closed_later] = <[_; 2]>::try_from(shops).unwrap();

    let mut products = Vec::new();
    for (name, status, deleted_at) in [
        ("ผักบุ้ง", product::Status::Hidden, None),
        ("ไข่เป็ด", product::Status::Deleted, Some(past(30))),
        ("น้ำพริก", product::Status::Deleted, Some(days_ago(30))),
    ] {
        let mut product = new_product(market.shop.id, market.category.id, name, None);
        product.status = Set(status);
        product.deleted_at = Set(deleted_at);
        products.push(product.insert(&db).await.unwrap());
    }
    let [hidden, deleted, deleted_later] = <[_; 3]>::try_from(products).unwrap();
    // IDs are per table: only the image of an owner of the right type goes
    assert_eq!((closed.id, deleted.id), (hidden.id, closed_later.id));

    let mut kept = BTreeSet::new();
    for (owner_type, owner_id) in [
        (OwnerType::Shop, market.shop.id),
        (OwnerType::Shop, closed_later.id),
        (OwnerType::Product, market.product.id),
        (OwnerType::Product, hidden.id),
        (OwnerType::Product, deleted_later.id),
    ] {
        kept.insert(image(&db, owner_type, owner_id).await);
    }
    image(&db, OwnerType::Shop, closed.id).await;
    image(&db, OwnerType::Product, deleted.id).await;

    let deleted_images = apply_twice(&db).await;
    assert_eq!(
        deleted_images,
        Deleted {
            deleted_owner_images: 2,
            ..Default::default()
        }
    );
    assert_eq!(images(&db).await, kept);
    // The shops and products themselves are kept (DR-04)
    assert_eq!(shop::Entity::find().count(&db).await.unwrap(), 3);
    assert_eq!(product::Entity::find().count(&db).await.unwrap(), 4);
}

/// FR-ADM-07, §5.4: an announcement sent is kept, with its photo, as long as its audit entry;
/// one never sent is the scheduler's `delete-expired` job's to delete.
#[tokio::test]
async fn announcements_go_with_the_audit_log() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;

    let mut announcements = Vec::new();
    for sent_at in [Some(past(365)), Some(days_ago(365)), None] {
        let announcement = announcement::ActiveModel {
            admin_id: Set(market.buyer.id),
            text: Set("ตลาดนัดพรุ่งนี้".to_owned()),
            created_at: Set(past(365)),
            sent_at: Set(sent_at),
            recipients: Set(sent_at.map(|_| 3)),
            ..Default::default()
        };
        announcements.push(announcement.insert(&db).await.unwrap());
    }
    let [old, recent, unsent] = <[_; 3]>::try_from(announcements).unwrap();
    image(&db, OwnerType::Announcement, old.id).await;
    let kept = BTreeSet::from([
        image(&db, OwnerType::Announcement, recent.id).await,
        image(&db, OwnerType::Announcement, unsent.id).await,
    ]);

    let deleted = apply_twice(&db).await;
    assert_eq!(
        deleted,
        Deleted {
            announcements: 1,
            ..Default::default()
        }
    );
    assert_eq!(
        ids::<announcement::Entity>(&db, announcement::Column::Id).await,
        [recent.id, unsent.id]
    );
    assert_eq!(images(&db).await, kept);
}
