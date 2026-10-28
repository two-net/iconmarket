//! Expiring unanswered orders (FR-ORD-05, AC-11) against a database and a fake LINE: an order
//! still `PENDING` `order.pendingTimeoutMinutes` after it was placed expires, gives back its
//! stock, and its buyer and seller are told; nothing else changes, nothing happens twice, and
//! its seller accepting at the same moment has one winner. Each test starts a throwaway
//! PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone};
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, Config};
use crate::line::testing::{Fake, PUSH};
use crate::line::{Body, Message};
use crate::persistence::entity::order_event::{self, ActorType};
use crate::persistence::entity::{order_item, product, push_log, shop, user};
use crate::persistence::{orm, random_id, testing};
use crate::thai::Money;

const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

fn minutes(minutes: i64) -> TimeDelta {
    TimeDelta::minutes(minutes)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    templates: Templates,
    buyer: i64,
    shop: i64,
    eggs: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap();
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap();
    let shop = testing::new_shop(seller.id).insert(&db).await.unwrap();
    let category = testing::new_category("ไข่/นม").insert(&db).await.unwrap();
    // 2 of 8 reserved
    let eggs = testing::new_product(shop.id, category.id, "ไข่ไก่เบอร์ 2", Some(6))
        .insert(&db)
        .await
        .unwrap();
    let config = config::sample();
    assert_eq!(config.order.pending_timeout_minutes, 60);
    Setup {
        _container: container,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        buyer: buyer.id,
        shop: shop.id,
        eggs: eggs.id,
    }
}

impl Setup {
    /// Order `number` of 2 แผง of eggs, placed at `at` and now at `status`.
    async fn order(&self, number: &str, status: Status, at: DateTime<Utc>) -> order::Model {
        let order = order::ActiveModel {
            status: Set(status),
            created_at: Set(at),
            updated_at: Set(at),
            ..testing::new_order(self.buyer, self.shop, number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap();
        order_item::ActiveModel {
            order_id: Set(order.id),
            product_id: Set(self.eggs),
            name: Set("ไข่ไก่เบอร์ 2".to_owned()),
            unit: Set("แผง".to_owned()),
            unit_price: Set(Money::from_baht(110)),
            quantity: Set(2),
            line_total: Set(Money::from_baht(220)),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .unwrap();
        order
    }

    async fn expire_at(&self, now: DateTime<Utc>) -> u64 {
        let (order, notification) = (&self.config.order, &self.config.notification);
        let line = self.line.client();
        expire(&self.db, &line, &self.templates, order, notification, now)
            .await
            .unwrap()
    }

    async fn get(&self, id: i64) -> order::Model {
        order::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn stock(&self) -> Option<i32> {
        let eggs = product::Entity::find_by_id(self.eggs).one(&self.db);
        eggs.await.unwrap().unwrap().stock
    }

    /// The pushes LINE got: each one's recipient and messages.
    fn pushes(&self) -> Vec<(String, serde_json::Value)> {
        self.line
            .requests()
            .iter()
            .filter(|request| request.path == PUSH)
            .map(|request| {
                let body = request.json();
                (
                    body["to"].as_str().unwrap().to_owned(),
                    body["messages"].clone(),
                )
            })
            .collect()
    }
}

fn sent(message: Message) -> serde_json::Value {
    serde_json::to_value([message]).unwrap()
}

/// AC-11, FR-ORD-05: an order still `PENDING` 60 minutes after it was placed expires when the
/// job runs, its stock is given back, the move is the system's in its history, and its buyer
/// and seller are told, the buyer with "เลือกซื้อต่อ".
#[tokio::test]
async fn an_unanswered_order_expires_and_both_parties_are_told() {
    let s = setup().await;
    let placed = s.order("IM-20261025-0001", Status::Pending, t0()).await;
    // Placed a minute later: not yet
    let later = s
        .order("IM-20261025-0002", Status::Pending, t0() + minutes(1))
        .await;
    let now = t0() + minutes(60);

    assert_eq!(s.expire_at(now).await, 1);
    let order = s.get(placed.id).await;
    assert_eq!(
        (order.status, order.expired_at),
        (Status::Expired, Some(now))
    );
    assert_eq!(s.get(later.id).await, later);
    assert_eq!(s.stock().await, Some(8));
    let history = order_event::Entity::find()
        .filter(order_event::Column::OrderId.eq(placed.id))
        .all(&s.db)
        .await
        .unwrap();
    let history: Vec<_> = history
        .into_iter()
        .map(|event| {
            (
                event.from_status,
                event.to_status,
                event.actor_type,
                event.actor_id,
            )
        })
        .collect();
    assert_eq!(
        history,
        [(
            Some(Status::Pending),
            Status::Expired,
            ActorType::System,
            None
        )]
    );

    let carry_on = vec![crate::orders::cart::continue_shopping()];
    let to_buyer = s
        .templates
        .order_expired_notice("ร้านป้าแดง", "IM-20261025-0001", 60, carry_on);
    let to_seller = s
        .templates
        .order_expired_seller_notice("IM-20261025-0001", 60);
    assert_eq!(
        s.pushes(),
        [
            (BUYER.to_owned(), sent(to_buyer.clone())),
            (SELLER.to_owned(), sent(to_seller.clone())),
        ]
    );
    let logged = push_log::Entity::find().all(&s.db).await.unwrap();
    let logged: Vec<_> = logged
        .into_iter()
        .map(|row| (row.push_type, row.status))
        .collect();
    assert_eq!(
        logged,
        [
            ("order.status".to_owned(), push_log::Status::Sent),
            ("order.expired".to_owned(), push_log::Status::Sent),
        ]
    );
    let Body::Flex { alt_text, .. } = &to_buyer.body else {
        panic!("not a card");
    };
    assert_eq!(alt_text, "ออเดอร์ IM-20261025-0001 หมดเวลาค่ะ");

    // Run again: nothing more, until the later one is due too
    assert_eq!(s.expire_at(now).await, 0);
    assert_eq!(s.pushes().len(), 2);
    assert_eq!(s.expire_at(now + minutes(1)).await, 1);
    assert_eq!(s.get(later.id).await.status, Status::Expired);
    assert_eq!(s.stock().await, Some(10));
}

/// FR-ORD-05: only `PENDING` orders expire; one answered, or final, stays as it is however old.
#[tokio::test]
async fn only_unanswered_orders_expire() {
    let s = setup().await;
    let old = t0() - TimeDelta::days(2);
    let mut kept = Vec::new();
    for (i, status) in [
        Status::Accepted,
        Status::Ready,
        Status::Delivering,
        Status::Completed,
        Status::Rejected,
        Status::Cancelled,
        Status::Expired,
    ]
    .into_iter()
    .enumerate()
    {
        let number = format!("IM-20261023-{:04}", i + 1);
        kept.push(s.order(&number, status, old).await);
    }
    assert_eq!(s.expire_at(t0()).await, 0);
    for order in kept {
        assert_eq!(s.get(order.id).await, order);
    }
    assert_eq!(s.stock().await, Some(6));
    assert!(s.pushes().is_empty());
}

/// Table 4.8-A, FR-ORD-08: with `notification.orderStatusToBuyer` off, only the seller is told
/// an order expired.
#[tokio::test]
async fn the_seller_is_always_told_an_order_expired() {
    let mut s = setup().await;
    s.config.notification.order_status_to_buyer = false;
    s.order("IM-20261025-0001", Status::Pending, t0()).await;
    assert_eq!(s.expire_at(t0() + minutes(90)).await, 1);
    let to: Vec<String> = s.pushes().into_iter().map(|(to, _)| to).collect();
    assert_eq!(to, [SELLER]);
}

/// FR-ORD-02: the seller accepting just as the order expires has one winner: accepted, it
/// stays accepted and keeps its stock; expired, the acceptance is refused.
#[tokio::test]
async fn accepting_as_the_order_expires_has_one_winner() {
    let s = setup().await;
    let seller: i64 = shop::Entity::find_by_id(s.shop)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .owner_id;
    for round in 0..5 {
        let number = format!("IM-20261025-{:04}", round + 1);
        let placed = s.order(&number, Status::Pending, t0()).await;
        let before = s.stock().await.unwrap();
        let accept = async {
            let txn = s.db.begin().await.unwrap();
            let by = By::user(ActorType::Seller, seller);
            let moved = lifecycle::transition(&txn, placed.id, Status::Accepted, by, None, t0())
                .await
                .unwrap();
            txn.commit().await.unwrap();
            moved
        };
        let (expired, accepted) = tokio::join!(s.expire_at(t0() + minutes(60)), accept);
        let order = s.get(placed.id).await;
        match order.status {
            Status::Expired => {
                assert_eq!(expired, 1, "round {round}");
                assert_eq!(accepted, Moved::Refused(order), "round {round}");
                assert_eq!(s.stock().await, Some(before + 2), "round {round}");
            }
            Status::Accepted => {
                assert_eq!(expired, 0, "round {round}");
                assert!(matches!(accepted, Moved::Done { .. }), "round {round}");
                assert_eq!(s.stock().await, Some(before), "round {round}");
            }
            other => panic!("round {round}: {other:?}"),
        }
    }
}

/// A seller who blocked the OA is not pushed to (FR-NOT-02); the buyer still is.
#[tokio::test]
async fn no_push_goes_to_a_seller_who_blocked_the_oa() {
    let s = setup().await;
    let blocked = user::ActiveModel {
        is_following: Set(false),
        ..Default::default()
    };
    user::Entity::update_many()
        .set(blocked)
        .filter(user::Column::LineUserId.eq(SELLER))
        .exec(&s.db)
        .await
        .unwrap();
    let placed = s.order("IM-20261025-0001", Status::Pending, t0()).await;
    assert_eq!(s.expire_at(t0() + minutes(60)).await, 1);
    assert_eq!(s.get(placed.id).await.status, Status::Expired);
    let to: Vec<String> = s.pushes().into_iter().map(|(to, _)| to).collect();
    assert_eq!(to, [BUYER]);
}
