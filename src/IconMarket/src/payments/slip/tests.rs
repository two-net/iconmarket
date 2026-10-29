//! Payment slips (FR-PAY-05): only the buyer of an order awaiting a PromptPay transfer sends a
//! slip, which keeps its photo behind a link that lasts `image.slipLinkHours` (AC-14) and has the
//! order await its seller's confirmation; the seller's "ยังไม่ได้รับ" refuses the slip they were
//! shown, and no other, and recording the order paid confirms it. Each takes effect once.
//! The tests each start a throwaway PostgreSQL, so they need Docker.

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{TimeDelta, TimeZone};
use sea_orm::ActiveValue::Set;
use sea_orm::{DatabaseConnection, QueryOrder};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, ImageConfig};
use crate::orders::lifecycle::By;
use crate::payments::{Recorded, record};
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::order::Status::{
    Accepted, Cancelled, Completed, Delivering, Pending, Ready,
};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::testing::{self, Market};
use crate::persistence::{orm, random_id};

use PaymentMethod::{Cash, PromptPay};
use PaymentStatus::{AwaitingConfirmation, Paid, Unpaid};

/// 25 ต.ค. 2569 10:00 in Bangkok
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 3, 0, 0).unwrap()
}

fn image_config() -> ImageConfig {
    config::sample().image
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    market: Market,
    other_buyer: i64,
}

/// The daily sequence of the orders the tests place
static NUMBERS: AtomicU32 = AtomicU32::new(1);

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = testing::market(&db).await;
    let other_buyer = testing::new_user("U-other-buyer").insert(&db).await;
    Setup {
        _container: container,
        db,
        market,
        other_buyer: other_buyer.unwrap().id,
    }
}

impl Setup {
    /// An order by the market's buyer paid by `method`, now `status` and `payment`.
    async fn order(
        &self,
        status: order::Status,
        method: PaymentMethod,
        payment: PaymentStatus,
    ) -> order::Model {
        let number = format!("IM-20261025-{:04}", NUMBERS.fetch_add(1, Ordering::Relaxed));
        let buyer = self.market.buyer.id;
        order::ActiveModel {
            status: Set(status),
            payment_method: Set(method),
            payment_status: Set(payment),
            ..testing::new_order(buyer, self.market.shop.id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap()
    }

    /// A photo user `user_id` sent, kept as their draft.
    async fn draft(&self, user_id: i64) -> Uuid {
        image::ActiveModel {
            id: Set(random_id()),
            owner_type: Set(OwnerType::Draft),
            owner_id: Set(user_id),
            position: Set(0),
            content_type: Set("image/jpeg".to_owned()),
            width: Set(1),
            height: Set(1),
            size_bytes: Set(3),
            data: Set(vec![0xff, 0xd8, 0xff]),
            created_at: Set(now()),
        }
        .insert(&self.db)
        .await
        .unwrap()
        .id
    }

    /// User `buyer_id` sends `photo` as the slip of order `id`, with a link made now.
    async fn submits(&self, id: i64, buyer_id: i64, photo: Uuid) -> (Submitted, SlipLink) {
        let link = SlipLink::new(now(), &image_config());
        let txn = self.db.begin().await.unwrap();
        let submitted = submit(&txn, id, buyer_id, photo, link, now())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        (submitted, link)
    }

    /// The market's buyer sends a photo as the slip of order `id`, which must be taken.
    async fn sent(&self, id: i64) -> payment_slip::Model {
        let buyer = self.market.buyer.id;
        let photo = self.draft(buyer).await;
        match self.submits(id, buyer, photo).await.0 {
            Submitted::Done { slip, .. } => slip,
            other => panic!("not sent: {other:?}"),
        }
    }

    async fn not_received(&self, id: i64, slip: Option<i64>) -> Answered {
        let txn = self.db.begin().await.unwrap();
        let answered = not_received(&txn, id, slip, now()).await.unwrap();
        txn.commit().await.unwrap();
        answered
    }

    async fn get(&self, id: i64) -> order::Model {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Order `id`'s slips, oldest first.
    async fn slips(&self, id: i64) -> Vec<payment_slip::Model> {
        payment_slip::Entity::find()
            .filter(payment_slip::Column::OrderId.eq(id))
            .order_by_asc(payment_slip::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn image(&self, id: Uuid) -> Option<image::Model> {
        image::Entity::find_by_id(id).one(&self.db).await.unwrap()
    }
}

/// FR-PAY-05, AC-14: the buyer's slip keeps their photo as its image, behind a link that lasts
/// `image.slipLinkHours`, and the order awaits its seller's confirmation, wherever it is on its
/// way.
#[tokio::test]
async fn a_slip_sent_awaits_the_sellers_confirmation() {
    let s = setup().await;
    for status in [Accepted, Ready, Delivering] {
        let placed = s.order(status, PromptPay, Unpaid).await;
        let photo = s.draft(s.market.buyer.id).await;
        let (submitted, link) = s.submits(placed.id, s.market.buyer.id, photo).await;

        let order = s.get(placed.id).await;
        let [slip] = <[_; 1]>::try_from(s.slips(placed.id).await).unwrap();
        assert_eq!(
            submitted,
            Submitted::Done {
                order: order.clone(),
                slip: slip.clone()
            }
        );
        assert_eq!(order.payment_status, AwaitingConfirmation);
        assert_eq!(order.updated_at, now());
        assert_eq!(order.status, status);
        assert_eq!(slip.status, Status::Submitted);
        assert_eq!(slip.image_id, Some(photo));
        assert_eq!(slip.access_token, link.token);
        assert_eq!(slip.token_expires_at, now() + TimeDelta::hours(24));
        assert_eq!(slip.created_at, now());
        // No longer a draft, so no sweep deletes it, and only its link shows it
        let image = s.image(photo).await.unwrap();
        assert_eq!(
            (image.owner_type, image.owner_id),
            (OwnerType::Slip, slip.id)
        );
        assert!(!awaits_transfer(&order));
    }
}

/// FR-PAY-05, FR-UI-13: only the buyer of an order accepted and awaiting a PromptPay transfer
/// sends its slip, once; anything else changes nothing and leaves the photo a draft.
#[tokio::test]
async fn only_an_order_awaiting_a_transfer_takes_a_slip_from_its_buyer() {
    let s = setup().await;
    let buyer = s.market.buyer.id;
    for (status, method, payment) in [
        (Pending, PromptPay, Unpaid),
        (Completed, PromptPay, Unpaid),
        (Cancelled, PromptPay, Unpaid),
        (Accepted, Cash, Unpaid),
        (Accepted, PromptPay, AwaitingConfirmation),
        (Accepted, PromptPay, Paid),
    ] {
        let placed = s.order(status, method, payment).await;
        assert!(
            !awaits_transfer(&placed),
            "{status:?} {method:?} {payment:?}"
        );
        let photo = s.draft(buyer).await;
        let (submitted, _) = s.submits(placed.id, buyer, photo).await;
        assert_eq!(submitted, Submitted::Refused(placed.clone()));
        assert_eq!(s.get(placed.id).await, placed);
        assert!(s.slips(placed.id).await.is_empty());
        assert_eq!(s.image(photo).await.unwrap().owner_type, OwnerType::Draft);
    }

    // Someone else's order
    let placed = s.order(Accepted, PromptPay, Unpaid).await;
    let photo = s.draft(s.other_buyer).await;
    let (submitted, _) = s.submits(placed.id, s.other_buyer, photo).await;
    assert_eq!(submitted, Submitted::Refused(placed.clone()));
    // Nor is someone else's photo taken for theirs
    let (submitted, _) = s.submits(placed.id, buyer, photo).await;
    assert_eq!(submitted, Submitted::Gone);
    assert_eq!(s.get(placed.id).await, placed);
    assert!(s.slips(placed.id).await.is_empty());

    // Sent once, a second time finds it awaiting confirmation already
    s.sent(placed.id).await;
    let again = s.draft(buyer).await;
    let order = s.get(placed.id).await;
    assert_eq!(
        s.submits(placed.id, buyer, again).await.0,
        Submitted::Refused(order)
    );
    assert_eq!(s.slips(placed.id).await.len(), 1);

    let (submitted, _) = s.submits(i64::MAX, buyer, again).await;
    assert_eq!(submitted, Submitted::NotFound);
}

/// A photo no longer there, swept as its flow ended, leaves no slip without one.
#[tokio::test]
async fn a_photo_gone_sends_no_slip() {
    let s = setup().await;
    let placed = s.order(Accepted, PromptPay, Unpaid).await;
    let (submitted, _) = s.submits(placed.id, s.market.buyer.id, random_id()).await;
    assert_eq!(submitted, Submitted::Gone);
    assert_eq!(s.get(placed.id).await, placed);
    assert!(s.slips(placed.id).await.is_empty());
}

/// FR-PAY-05: the seller's "ยังไม่ได้รับ" refuses the slip, and the order awaits a transfer again,
/// for the buyer to send another; tapped again, or on the notice of a slip since replaced, it
/// changes nothing.
#[tokio::test]
async fn not_received_refuses_the_slip_shown_and_no_other() {
    let s = setup().await;
    let placed = s.order(Delivering, PromptPay, Unpaid).await;
    let first = s.sent(placed.id).await;

    let answered = s.not_received(placed.id, Some(first.id)).await;
    let order = s.get(placed.id).await;
    assert_eq!(answered, Answered::Done(order.clone()));
    assert_eq!(order.payment_status, Unpaid);
    assert!(awaits_transfer(&order));
    assert_eq!(s.slips(placed.id).await[0].status, Status::Rejected);
    // Its image is kept, for retention to delete (§5.4)
    assert!(s.image(first.image_id.unwrap()).await.is_some());

    // Tapped again
    assert_eq!(
        s.not_received(placed.id, Some(first.id)).await,
        Answered::Refused(order)
    );

    // The buyer sends another; the first slip's notice no longer refuses anything
    let second = s.sent(placed.id).await;
    let order = s.get(placed.id).await;
    assert_eq!(
        s.not_received(placed.id, Some(first.id)).await,
        Answered::Refused(order)
    );
    assert_eq!(
        s.slips(placed.id).await[1].status,
        Status::Submitted,
        "{second:?}"
    );
    // Asked whether it was paid as it was handed over (FR-PAY-07), without a slip to name
    assert!(matches!(
        s.not_received(placed.id, None).await,
        Answered::Done(_)
    ));
    assert_eq!(s.slips(placed.id).await[1].status, Status::Rejected);

    assert_eq!(s.not_received(i64::MAX, None).await, Answered::NotFound);
}

/// FR-PAY-05, FR-PAY-08: "ได้รับเงินแล้ว" by the seller, or an admin, records the order paid,
/// which confirms the slip awaiting the answer and leaves those answered before as they were.
#[tokio::test]
async fn recording_the_order_paid_confirms_its_slip() {
    let s = setup().await;
    let placed = s.order(Accepted, PromptPay, Unpaid).await;
    let first = s.sent(placed.id).await;
    s.not_received(placed.id, Some(first.id)).await;
    s.sent(placed.id).await;

    let seller = By::user(ActorType::Seller, s.market.shop.owner_id);
    let txn = s.db.begin().await.unwrap();
    let recorded = record(&txn, placed.id, Paid, seller, now()).await.unwrap();
    txn.commit().await.unwrap();
    assert!(matches!(
        recorded,
        Recorded::Done {
            from: AwaitingConfirmation,
            ..
        }
    ));
    let statuses: Vec<_> = s
        .slips(placed.id)
        .await
        .iter()
        .map(|slip| slip.status)
        .collect();
    assert_eq!(statuses, [Status::Rejected, Status::Confirmed]);
    assert_eq!(s.get(placed.id).await.payment_status, Paid);
}
