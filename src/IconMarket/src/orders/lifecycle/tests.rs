//! The order state machine (NFR-MNT-07): every move of Table 4.6-B is allowed to the actors it
//! names and no other move is, each takes effect once with its history and stock, and of moves
//! made at the same moment exactly one wins (FR-ORD-02).

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, Iterable, QueryFilter,
    QueryOrder, TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::persistence::entity::order_event::ActorType::{Admin, Buyer, Seller, System};
use crate::persistence::entity::{order_event, order_item, product, user};
use crate::persistence::testing::{self, Market};
use crate::persistence::{orm, random_id};
use crate::thai::Money;

use Fulfillment::{Delivery, Pickup};
use Status::{Accepted, Cancelled, Completed, Delivering, Expired, Pending, Ready, Rejected};

/// Table 4.6-B, written out: from, to, by, and the only fulfillment it applies to, if any.
const TABLE: [(Status, Status, ActorType, Option<Fulfillment>); 18] = [
    (Pending, Accepted, Seller, None),
    (Pending, Rejected, Seller, None),
    (Pending, Cancelled, Buyer, None),
    (Pending, Cancelled, Admin, None),
    (Pending, Expired, System, None),
    (Accepted, Ready, Seller, Some(Pickup)),
    (Accepted, Delivering, Seller, Some(Delivery)),
    (Accepted, Completed, Seller, None),
    (Ready, Completed, Seller, None),
    (Ready, Completed, Buyer, None),
    (Delivering, Completed, Seller, None),
    (Delivering, Completed, Buyer, None),
    (Accepted, Cancelled, Seller, None),
    (Ready, Cancelled, Seller, None),
    (Delivering, Cancelled, Seller, None),
    (Accepted, Cancelled, Admin, None),
    (Ready, Cancelled, Admin, None),
    (Delivering, Cancelled, Admin, None),
];

fn in_table(from: Status, to: Status, by: ActorType, fulfillment: Fulfillment) -> bool {
    TABLE.iter().any(|&(f, t, b, only)| {
        (f, t, b) == (from, to, by) && only.is_none_or(|only| only == fulfillment)
    })
}

/// Every (from, to, by, fulfillment) there is.
fn every_move() -> impl Iterator<Item = (Status, Status, ActorType, Fulfillment)> {
    Status::iter().flat_map(|from| {
        Status::iter().flat_map(move |to| {
            ActorType::iter().flat_map(move |by| {
                Fulfillment::iter().map(move |fulfillment| (from, to, by, fulfillment))
            })
        })
    })
}

/// NFR-MNT-07, FR-ORD-02: of all 512 moves, those of Table 4.6-B, by the actors it names, are
/// allowed, and every other is forbidden.
#[test]
fn only_the_moves_of_table_4_6_b_are_allowed() {
    let mut allowed_moves = 0;
    for (from, to, by, fulfillment) in every_move() {
        let expected = in_table(from, to, by, fulfillment);
        assert_eq!(
            allowed(from, to, by, fulfillment),
            expected,
            "{from:?} → {to:?} by {by:?}, {fulfillment:?}"
        );
        allowed_moves += usize::from(expected);
    }
    assert_eq!(every_move().count(), 512);
    // Each row twice, once per fulfillment, but READY and DELIVERING once each
    assert_eq!(allowed_moves, TABLE.len() * 2 - 2);
}

/// Table 4.6-A: a final status moves no further, and every other has somewhere to go.
#[test]
fn final_statuses_move_no_further() {
    for from in Status::iter() {
        let moves = every_move()
            .filter(|&(f, to, by, fulfillment)| f == from && allowed(from, to, by, fulfillment));
        assert_eq!(moves.count() == 0, from.is_final(), "{from:?}");
    }
}

/// FR-ORD-04: rejecting, and cancelling as the seller or an admin, need a reason; nothing else
/// does.
#[test]
fn rejecting_and_cancelling_by_the_shop_or_an_admin_need_a_reason() {
    for to in Status::iter() {
        for by in ActorType::iter() {
            let expected = to == Rejected || (to == Cancelled && matches!(by, Seller | Admin));
            assert_eq!(needs_reason(to, by), expected, "{to:?} by {by:?}");
        }
    }
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
    /// Of unlimited stock
    made_to_order: product::Model,
}

/// The daily sequence of the orders the tests place
static NUMBERS: AtomicU32 = AtomicU32::new(1);

/// How many of `market.product` each order holds, and how many are left in stock once
/// reserved.
const QUANTITY: i32 = 2;
const LEFT: i32 = 6;

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = testing::market(&db).await;
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user("U-admin")
    };
    let admin = admin.insert(&db).await.unwrap().id;
    let made_to_order = testing::new_product(market.shop.id, market.category.id, "ขนมครก", None)
        .insert(&db)
        .await
        .unwrap();
    Setup {
        _container: container,
        seller: market.shop.owner_id,
        db,
        market,
        admin,
        made_to_order,
    }
}

impl Setup {
    /// An order of [`QUANTITY`] of the market's product, reserved, and one made to order, now
    /// at `status`, received by `fulfillment`.
    async fn order(&self, status: Status, fulfillment: Fulfillment) -> order::Model {
        let number = format!("IM-20261025-{:04}", NUMBERS.fetch_add(1, Ordering::Relaxed));
        let order = order::ActiveModel {
            status: Set(status),
            fulfillment: Set(fulfillment),
            ..testing::new_order(
                self.market.buyer.id,
                self.market.shop.id,
                &number,
                random_id(),
            )
        }
        .insert(&self.db)
        .await
        .unwrap();
        for (product, quantity) in [(&self.market.product, QUANTITY), (&self.made_to_order, 1)] {
            order_item::ActiveModel {
                order_id: Set(order.id),
                product_id: Set(product.id),
                name: Set(product.name.clone()),
                unit: Set(product.unit.clone()),
                unit_price: Set(product.price),
                quantity: Set(quantity),
                line_total: Set(product.price.times(quantity)),
                ..Default::default()
            }
            .insert(&self.db)
            .await
            .unwrap();
        }
        self.set_stock(Some(LEFT)).await;
        order
    }

    async fn set_stock(&self, stock: Option<i32>) {
        let row = product::ActiveModel {
            stock: Set(stock),
            ..Default::default()
        };
        product::Entity::update_many()
            .set(row)
            .filter(product::Column::Id.eq(self.market.product.id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    async fn stocks(&self) -> (Option<i32>, Option<i32>) {
        let stock = |id| async move {
            let found = product::Entity::find_by_id(id).one(&self.db).await.unwrap();
            found.unwrap().stock
        };
        (
            stock(self.market.product.id).await,
            stock(self.made_to_order.id).await,
        )
    }

    async fn moves(
        &self,
        id: i64,
        to: Status,
        by: By,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Moved {
        let txn = self.db.begin().await.unwrap();
        let moved = transition(&txn, id, to, by, reason, now).await.unwrap();
        txn.commit().await.unwrap();
        moved
    }

    async fn get(&self, id: i64) -> order::Model {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Order `id`'s history: (from, to, actor type, actor, reason, time), oldest first.
    async fn history(&self, id: i64) -> Vec<Event> {
        order_event::Entity::find()
            .filter(order_event::Column::OrderId.eq(id))
            .order_by_asc(order_event::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|event| {
                (
                    event.from_status,
                    event.to_status,
                    event.actor_type,
                    event.actor_id,
                    event.reason,
                    event.created_at,
                )
            })
            .collect()
    }

    /// Who moves an order as `by`.
    fn by(&self, by: ActorType) -> By {
        match by {
            Buyer => By::user(Buyer, self.market.buyer.id),
            Seller => By::user(Seller, self.seller),
            Admin => By::user(Admin, self.admin),
            System => By::SYSTEM,
        }
    }
}

type Event = (
    Option<Status>,
    Status,
    ActorType,
    Option<i64>,
    Option<String>,
    DateTime<Utc>,
);

/// When `order` reached `status`, as stamped.
fn reached(order: &order::Model, status: Status) -> Option<DateTime<Utc>> {
    match status {
        Pending => Some(order.created_at),
        Accepted => order.accepted_at,
        Ready => order.ready_at,
        Delivering => order.delivering_at,
        Completed => order.completed_at,
        Rejected => order.rejected_at,
        Cancelled => order.cancelled_at,
        Expired => order.expired_at,
    }
}

/// FR-ORD-02, FR-ORD-04, NFR-MNT-07: every allowed move takes the order there, stamped with
/// when, keeps the reason of a rejection or cancellation, is recorded in its history with
/// actor, time and reason, and gives back the reserved stock only if the order will never be
/// handed over, leaving unlimited stock as it is.
#[tokio::test]
async fn every_allowed_move_takes_effect_with_its_history_and_stock() {
    let s = setup().await;
    for (from, to, by, only) in TABLE {
        let fulfillment = only.unwrap_or(if from == Delivering { Delivery } else { Pickup });
        let placed = s.order(from, fulfillment).await;
        let reason = needs_reason(to, by).then_some("สินค้าหมด");
        let at = now() + TimeDelta::minutes(5);
        let what = format!("{from:?} → {to:?} by {by:?}");

        let moved = s.moves(placed.id, to, s.by(by), reason, at).await;
        let order = s.get(placed.id).await;
        assert_eq!(
            moved,
            Moved::Done {
                from,
                order: order.clone()
            },
            "{what}"
        );
        assert_eq!(order.status, to, "{what}");
        assert_eq!(reached(&order, to), Some(at), "{what}");
        assert_eq!(order.updated_at, at, "{what}");
        assert_eq!(order.cancel_reason.as_deref(), reason, "{what}");
        assert_eq!(order.payment_status, PaymentStatus::Unpaid, "{what}");
        let by = s.by(by);
        assert_eq!(
            s.history(placed.id).await,
            [(
                Some(from),
                to,
                by.actor,
                by.id,
                reason.map(str::to_owned),
                at
            )],
            "{what}"
        );
        let restocked = matches!(to, Rejected | Cancelled | Expired);
        let stock = if restocked { LEFT + QUANTITY } else { LEFT };
        assert_eq!(s.stocks().await, (Some(stock), None), "{what}");
    }
}

/// FR-ORD-02: a move not in Table 4.6-B, from where the order is, by whom, for its fulfillment,
/// is refused with the order as it is, and changes nothing: not the order, its history or the
/// stock.
#[tokio::test]
async fn every_forbidden_move_changes_nothing() {
    let s = setup().await;
    for fulfillment in Fulfillment::iter() {
        for from in Status::iter() {
            let placed = s.order(from, fulfillment).await;
            for to in Status::iter().filter(|&to| to != Pending) {
                for by in ActorType::iter() {
                    if allowed(from, to, by, fulfillment) {
                        continue;
                    }
                    let reason = needs_reason(to, by).then_some("สินค้าหมด");
                    let moved = s.moves(placed.id, to, s.by(by), reason, now()).await;
                    let what = format!("{from:?} → {to:?} by {by:?}, {fulfillment:?}");
                    assert_eq!(moved, Moved::Refused(placed.clone()), "{what}");
                }
            }
            assert_eq!(s.get(placed.id).await, placed);
            assert_eq!(s.history(placed.id).await, []);
            assert_eq!(s.stocks().await, (Some(LEFT), None));
        }
    }
}

/// FR-ORD-04: an allowed move is refused without the reason it needs, or with one it doesn't
/// take; and a missing order is not found.
#[tokio::test]
async fn a_move_needs_its_reason_and_only_then() {
    let s = setup().await;
    let placed = s.order(Pending, Pickup).await;
    let unexplained = s
        .moves(placed.id, Rejected, s.by(Seller), None, now())
        .await;
    assert_eq!(unexplained, Moved::Refused(placed.clone()));
    let explained = s
        .moves(placed.id, Accepted, s.by(Seller), Some("สินค้าหมด"), now())
        .await;
    assert_eq!(explained, Moved::Refused(placed.clone()));
    assert_eq!(s.history(placed.id).await, []);

    let missing = s.moves(placed.id + 1_000, Accepted, s.by(Seller), None, now());
    assert_eq!(missing.await, Moved::NotFound);
}

/// FR-PAY-09: a paid order cancelled is to be refunded; one not paid, or whose slip awaits the
/// seller, stays as it is.
#[tokio::test]
async fn a_paid_order_cancelled_is_to_be_refunded() {
    let s = setup().await;
    for (paid, then) in [
        (PaymentStatus::Paid, PaymentStatus::RefundDue),
        (PaymentStatus::Unpaid, PaymentStatus::Unpaid),
        (
            PaymentStatus::AwaitingConfirmation,
            PaymentStatus::AwaitingConfirmation,
        ),
    ] {
        let placed = s.order(Accepted, Delivery).await;
        let mut row: order::ActiveModel = placed.into();
        row.payment_status = Set(paid);
        let placed = row.update(&s.db).await.unwrap();
        let reason = Some("ไม่สะดวกส่ง");
        let Moved::Done { order, .. } = s
            .moves(placed.id, Cancelled, s.by(Admin), reason, now())
            .await
        else {
            panic!("not cancelled");
        };
        assert_eq!(order.payment_status, then, "{paid:?}");
    }
    // Completing a paid order leaves it paid
    let placed = s.order(Ready, Pickup).await;
    let mut row: order::ActiveModel = placed.into();
    row.payment_status = Set(PaymentStatus::Paid);
    let placed = row.update(&s.db).await.unwrap();
    let Moved::Done { order, .. } = s
        .moves(placed.id, Completed, s.by(Buyer), None, now())
        .await
    else {
        panic!("not completed");
    };
    assert_eq!(order.payment_status, PaymentStatus::Paid);
}

/// A product that became of unlimited stock since keeps it, and one deleted since still gets its
/// stock back.
#[tokio::test]
async fn stock_is_given_back_only_where_it_is_counted() {
    let s = setup().await;
    let placed = s.order(Pending, Pickup).await;
    s.set_stock(None).await;
    s.moves(placed.id, Cancelled, s.by(Buyer), None, now())
        .await;
    assert_eq!(s.stocks().await, (None, None));

    let placed = s.order(Pending, Pickup).await;
    let row = product::ActiveModel {
        status: Set(product::Status::Deleted),
        ..Default::default()
    };
    product::Entity::update_many()
        .set(row)
        .filter(product::Column::Id.eq(s.market.product.id))
        .exec(&s.db)
        .await
        .unwrap();
    s.moves(placed.id, Expired, By::SYSTEM, None, now()).await;
    assert_eq!(s.stocks().await, (Some(LEFT + QUANTITY), None));
}

/// FR-ORD-02: the buyer cancelling while the seller accepts at the same moment has exactly one
/// winner. The other is refused with the order as the winner left it, the history holds the
/// winning move only, and the stock is given back only if the cancellation won.
#[tokio::test]
async fn the_buyer_cancelling_while_the_seller_accepts_has_one_winner() {
    let s = setup().await;
    for round in 0..5 {
        let placed = s.order(Pending, Delivery).await;
        let (cancelled, accepted) = tokio::join!(
            s.moves(placed.id, Cancelled, s.by(Buyer), None, now()),
            s.moves(placed.id, Accepted, s.by(Seller), None, now()),
        );
        let order = s.get(placed.id).await;
        let (won, lost) = match order.status {
            Cancelled => (cancelled, accepted),
            Accepted => (accepted, cancelled),
            other => panic!("round {round}: {other:?}"),
        };
        let done = Moved::Done {
            from: Pending,
            order: order.clone(),
        };
        assert_eq!(won, done, "round {round}");
        assert_eq!(lost, Moved::Refused(order.clone()), "round {round}");
        let history = s.history(placed.id).await;
        assert_eq!(history.len(), 1, "round {round}");
        assert_eq!(history[0].1, order.status, "round {round}");
        let stock = if order.status == Cancelled {
            LEFT + QUANTITY
        } else {
            LEFT
        };
        assert_eq!(s.stocks().await.0, Some(stock), "round {round}");
    }
}

/// Money is kept exactly once an order moves (DR-01).
#[tokio::test]
async fn amounts_are_unchanged_by_a_move() {
    let s = setup().await;
    let placed = s.order(Pending, Pickup).await;
    let Moved::Done { order, .. } = s
        .moves(placed.id, Accepted, s.by(Seller), None, now())
        .await
    else {
        panic!("not accepted");
    };
    assert_eq!(
        (order.subtotal, order.delivery_fee, order.total),
        (Money::from_baht(220), Money::ZERO, Money::from_baht(220))
    );
}
