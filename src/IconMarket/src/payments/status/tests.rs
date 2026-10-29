//! Payment status (FR-PAY-08): only an order's seller, or an admin, records it paid or refunded,
//! and only from where FR-PAY-07 and FR-PAY-09 allow; its buyer never does, whatever else they
//! are; and recording it while the order is cancelled has one winner.
//! The database tests each start a throwaway PostgreSQL, so they need Docker.

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::TimeZone;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Iterable, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::orders::lifecycle::{Moved, transition};
use crate::persistence::entity::order_event::ActorType::{Admin, Buyer, Seller, System};
use crate::persistence::entity::user;
use crate::persistence::testing::{self, Market};
use crate::persistence::{orm, random_id};

use PaymentMethod::{Cash, PromptPay};
use PaymentStatus::{AwaitingConfirmation, Paid, RefundDue, Refunded, Unpaid};
use Status::{Accepted, Cancelled, Completed, Delivering, Expired, Pending, Ready, Rejected};

/// FR-PAY-08, written out: from, to, the order statuses it applies at, and the only payment
/// method it applies to, if any. Each by the seller or an admin, and no one else.
const TABLE: [(
    PaymentStatus,
    PaymentStatus,
    &[Status],
    Option<PaymentMethod>,
); 5] = [
    // A transfer, once it arrives (FR-PAY-03)
    (
        Unpaid,
        Paid,
        &[Accepted, Ready, Delivering],
        Some(PromptPay),
    ),
    (
        AwaitingConfirmation,
        Paid,
        &[Accepted, Ready, Delivering],
        Some(PromptPay),
    ),
    // Once handed over (FR-PAY-07)
    (Unpaid, Paid, &[Completed], None),
    (AwaitingConfirmation, Paid, &[Completed], None),
    // Settled between the parties (FR-PAY-09), wherever the order is: only a cancelled one is
    // ever owed back
    (RefundDue, Refunded, &ALL, None),
];

const ALL: [Status; 8] = [
    Pending, Accepted, Ready, Delivering, Completed, Rejected, Cancelled, Expired,
];

fn in_table(
    from: PaymentStatus,
    to: PaymentStatus,
    by: ActorType,
    order: Status,
    method: PaymentMethod,
) -> bool {
    matches!(by, Seller | Admin)
        && TABLE.iter().any(|&(f, t, at, only)| {
            (f, t) == (from, to) && at.contains(&order) && only.is_none_or(|only| only == method)
        })
}

/// Every (from, to, by, order status, method) there is.
fn every_change() -> impl Iterator<
    Item = (
        PaymentStatus,
        PaymentStatus,
        ActorType,
        Status,
        PaymentMethod,
    ),
> {
    PaymentStatus::iter().flat_map(|from| {
        PaymentStatus::iter().flat_map(move |to| {
            ActorType::iter().flat_map(move |by| {
                Status::iter().flat_map(move |order| {
                    PaymentMethod::iter().map(move |method| (from, to, by, order, method))
                })
            })
        })
    })
}

/// FR-PAY-08: of every change there is, those written out above, by the seller or an admin, are
/// allowed, and no other.
#[test]
fn only_the_seller_or_an_admin_records_a_payment() {
    for (from, to, by, order, method) in every_change() {
        assert_eq!(
            allowed(from, to, by, order, method),
            in_table(from, to, by, order, method),
            "{from:?} → {to:?} by {by:?}, {order:?}, {method:?}"
        );
    }
}

/// The acceptance criterion of #35: a buyer never sets an order `PAID`, nor anything else, and
/// neither does the system by hand.
#[test]
fn a_buyer_never_records_a_payment() {
    for (from, to, by, order, method) in every_change() {
        if matches!(by, Buyer | System) {
            assert!(!allowed(from, to, by, order, method), "{from:?} → {to:?}");
        }
    }
}

/// Cash is paid on receipt: the seller records it once they hand the order over, not before.
#[test]
fn cash_is_recorded_once_handed_over() {
    for order in [Accepted, Ready, Delivering] {
        assert!(!allowed(Unpaid, Paid, Seller, order, Cash), "{order:?}");
        assert!(allowed(Unpaid, Paid, Seller, order, PromptPay), "{order:?}");
    }
    assert!(allowed(Unpaid, Paid, Seller, Completed, Cash));
}

/// 25 ต.ค. 2569 10:00 in Bangkok
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 3, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    market: Market,
    seller: i64,
    admin: i64,
}

/// The daily sequence of the orders the tests place
static NUMBERS: AtomicU32 = AtomicU32::new(1);

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = testing::market(&db).await;
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user("U-admin")
    };
    let admin = admin.insert(&db).await.unwrap().id;
    Setup {
        _container: container,
        seller: market.shop.owner_id,
        db,
        market,
        admin,
    }
}

impl Setup {
    /// An order by `buyer_id` paid by `method`, now `status` and `payment`.
    async fn order_by(
        &self,
        buyer_id: i64,
        status: Status,
        method: PaymentMethod,
        payment: PaymentStatus,
    ) -> order::Model {
        let number = format!("IM-20261025-{:04}", NUMBERS.fetch_add(1, Ordering::Relaxed));
        order::ActiveModel {
            status: Set(status),
            payment_method: Set(method),
            payment_status: Set(payment),
            ..testing::new_order(buyer_id, self.market.shop.id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap()
    }

    /// An order by the market's buyer.
    async fn order(
        &self,
        status: Status,
        method: PaymentMethod,
        payment: PaymentStatus,
    ) -> order::Model {
        let buyer = self.market.buyer.id;
        self.order_by(buyer, status, method, payment).await
    }

    async fn records(&self, id: i64, to: PaymentStatus, by: By) -> Recorded {
        let txn = self.db.begin().await.unwrap();
        let recorded = record(&txn, id, to, by, now()).await.unwrap();
        txn.commit().await.unwrap();
        recorded
    }

    async fn get(&self, id: i64) -> order::Model {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    fn by(&self, by: ActorType) -> By {
        match by {
            Buyer => By::user(Buyer, self.market.buyer.id),
            Seller => By::user(Seller, self.seller),
            Admin => By::user(Admin, self.admin),
            System => By::SYSTEM,
        }
    }
}

/// FR-PAY-07, FR-PAY-09: the seller records a transfer as it arrives, cash once handed over, and
/// a refund once settled; each takes effect once, stamped, and leaves the order where it is.
#[tokio::test]
async fn the_seller_records_payments_and_refunds() {
    let s = setup().await;
    for (status, method, from, to) in [
        (Accepted, PromptPay, Unpaid, Paid),
        (Delivering, PromptPay, AwaitingConfirmation, Paid),
        (Completed, Cash, Unpaid, Paid),
        (Cancelled, PromptPay, RefundDue, Refunded),
    ] {
        let placed = s.order(status, method, from).await;
        let recorded = s.records(placed.id, to, s.by(Seller)).await;
        let order = s.get(placed.id).await;
        assert_eq!(
            recorded,
            Recorded::Done {
                from,
                order: order.clone()
            }
        );
        assert_eq!(order.payment_status, to);
        assert_eq!(order.updated_at, now());
        assert_eq!(order.status, status);

        // Tapped again, it finds it recorded already
        let again = s.records(placed.id, to, s.by(Seller)).await;
        assert_eq!(again, Recorded::Refused(order));
    }
}

/// FR-PAY-08: an admin records a payment too, but not of their own order; the buyer never does;
/// and nothing is recorded where it isn't allowed. A refused change changes nothing.
#[tokio::test]
async fn a_payment_is_recorded_only_by_whom_and_where_it_may_be() {
    let s = setup().await;
    let placed = s.order(Completed, Cash, Unpaid).await;
    assert!(matches!(
        s.records(placed.id, Paid, s.by(Admin)).await,
        Recorded::Done { .. }
    ));

    // The acceptance criterion of #35
    let placed = s.order(Completed, Cash, Unpaid).await;
    let refused = s.records(placed.id, Paid, s.by(Buyer)).await;
    assert_eq!(refused, Recorded::Refused(placed.clone()));
    assert_eq!(s.get(placed.id).await, placed);
    // Not even claiming to be its seller
    let as_seller = By::user(Seller, s.market.buyer.id);
    assert_eq!(
        s.records(placed.id, Paid, as_seller).await,
        Recorded::Refused(placed.clone())
    );
    // An admin who bought it is its buyer
    let own = s.order_by(s.admin, Completed, Cash, Unpaid).await;
    assert_eq!(
        s.records(own.id, Paid, s.by(Admin)).await,
        Recorded::Refused(own.clone())
    );

    for (status, method, from, to) in [
        (Pending, PromptPay, Unpaid, Paid),
        (Accepted, Cash, Unpaid, Paid),
        (Cancelled, PromptPay, Unpaid, Paid),
        (Expired, Cash, Unpaid, Paid),
        (Completed, Cash, Paid, Refunded),
        (Completed, Cash, Paid, Unpaid),
        (Cancelled, Cash, RefundDue, Paid),
        (Cancelled, Cash, Refunded, RefundDue),
    ] {
        let placed = s.order(status, method, from).await;
        let refused = s.records(placed.id, to, s.by(Seller)).await;
        assert_eq!(
            refused,
            Recorded::Refused(placed.clone()),
            "{status:?} {from:?}"
        );
        assert_eq!(s.get(placed.id).await, placed);
    }

    let missing = s.records(placed.id + 1_000, Paid, s.by(Seller)).await;
    assert_eq!(missing, Recorded::NotFound);
}

/// FR-PAY-09, FR-ORD-02: the seller recording a transfer while an admin cancels the order has
/// exactly one winner. If the payment won, the cancellation leaves it to be refunded; if the
/// cancellation won, the payment is refused and nothing is owed.
#[tokio::test]
async fn recording_a_payment_while_the_order_is_cancelled_has_one_winner() {
    let s = setup().await;
    for round in 0..5 {
        let placed = s.order(Delivering, PromptPay, Unpaid).await;
        let cancel = async {
            let txn = s.db.begin().await.unwrap();
            let reason = Some("ร้านไม่ส่งของ");
            let moved = transition(&txn, placed.id, Cancelled, s.by(Admin), reason, now());
            let moved = moved.await.unwrap();
            txn.commit().await.unwrap();
            moved
        };
        let (moved, recorded) = tokio::join!(cancel, s.records(placed.id, Paid, s.by(Seller)));
        let order = s.get(placed.id).await;
        assert_eq!(order.status, Cancelled, "round {round}");
        assert!(matches!(moved, Moved::Done { .. }), "round {round}");
        match recorded {
            // Paid first, so cancelled once paid
            Recorded::Done { .. } => assert_eq!(order.payment_status, RefundDue, "round {round}"),
            Recorded::Refused(seen) => {
                assert_eq!(seen, order, "round {round}");
                assert_eq!(order.payment_status, Unpaid, "round {round}");
            }
            Recorded::NotFound => panic!("round {round}: not found"),
        }
    }
}
