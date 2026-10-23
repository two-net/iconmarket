//! Databases for tests: a throwaway PostgreSQL per test, which needs Docker, or a pool that
//! never connects; and rows to fill them with.

use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::{ActiveModelTrait, DatabaseConnection};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ContainerRequest, ImageExt};

use super::entity::{category, order, product, shop, user};
use super::{connect, migrate};
use crate::config::{DatabaseConfig, Secret};
use crate::thai::Money;

/// An empty database, and the container serving it.
pub async fn start(image: ContainerRequest<Postgres>) -> (ContainerAsync<Postgres>, PgPool) {
    let container = image.with_tag("18").start().await.expect(
        "cannot start PostgreSQL; is Docker running at DOCKER_HOST or /var/run/docker.sock?",
    );
    let pool = pool(&container).await;
    (container, pool)
}

/// Another pool on the database `container` serves, as a second instance of the app has.
pub async fn pool(container: &ContainerAsync<Postgres>) -> PgPool {
    let config = DatabaseConfig {
        host: container.get_host().await.unwrap().to_string(),
        port: container.get_host_port_ipv4(5432).await.unwrap(),
        name: "postgres".to_owned(),
        user: "postgres".to_owned(),
        password: Secret::new("postgres"),
        ssl_mode: PgSslMode::Disable,
        max_connections: 2,
    };
    connect(&config).await.unwrap()
}

/// A fresh database holding the schema, and the container serving it.
pub async fn migrated() -> (ContainerAsync<Postgres>, PgPool) {
    let (container, pool) = start(Postgres::default().into()).await;
    migrate(&pool).await.unwrap();
    (container, pool)
}

/// A pool whose database is always down: nothing listens on port 1.
pub fn unreachable() -> PgPool {
    let options = PgConnectOptions::new_without_pgpass()
        .host("127.0.0.1")
        .port(1);
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy_with(options)
}

/// 22 ต.ค. 2569 14:30 in Bangkok, with microseconds as PostgreSQL keeps them.
pub fn at() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap() + TimeDelta::microseconds(123_456)
}

pub fn new_user(line_user_id: &str) -> user::ActiveModel {
    user::ActiveModel {
        line_user_id: Set(Some(line_user_id.to_owned())),
        is_admin: Set(false),
        is_following: Set(true),
        status: Set(user::Status::Active),
        created_at: Set(at()),
        updated_at: Set(at()),
        ..Default::default()
    }
}

pub fn new_shop(owner_id: i64) -> shop::ActiveModel {
    shop::ActiveModel {
        owner_id: Set(owner_id),
        name: Set("ร้านป้าแดง".to_owned()),
        description: Set("ไข่ไก่ ผักสวนครัว".to_owned()),
        status: Set(shop::Status::Approved),
        is_open: Set(true),
        offers_pickup: Set(true),
        offers_delivery: Set(true),
        delivery_fee: Set(Money::from_baht(10)),
        show_phone: Set(true),
        created_at: Set(at()),
        updated_at: Set(at()),
        ..Default::default()
    }
}

pub fn new_category(name: &str) -> category::ActiveModel {
    category::ActiveModel {
        name: Set(name.to_owned()),
        sort_order: Set(1),
        is_active: Set(true),
        ..Default::default()
    }
}

pub fn new_product(
    shop_id: i64,
    category_id: i64,
    name: &str,
    stock: Option<i32>,
) -> product::ActiveModel {
    product::ActiveModel {
        shop_id: Set(shop_id),
        category_id: Set(category_id),
        name: Set(name.to_owned()),
        price: Set(Money::from_baht(110)),
        unit: Set("แผง".to_owned()),
        stock: Set(stock),
        status: Set(product::Status::Active),
        created_at: Set(at()),
        updated_at: Set(at()),
        ..Default::default()
    }
}

pub fn new_order(
    buyer_id: i64,
    shop_id: i64,
    number: &str,
    checkout_key: Uuid,
) -> order::ActiveModel {
    order::ActiveModel {
        order_number: Set(number.to_owned()),
        checkout_key: Set(checkout_key),
        buyer_id: Set(buyer_id),
        shop_id: Set(shop_id),
        status: Set(order::Status::Pending),
        fulfillment: Set(order::Fulfillment::Pickup),
        subtotal: Set(Money::from_baht(220)),
        delivery_fee: Set(Money::ZERO),
        total: Set(Money::from_baht(220)),
        payment_method: Set(order::PaymentMethod::Cash),
        payment_status: Set(order::PaymentStatus::Unpaid),
        created_at: Set(at()),
        updated_at: Set(at()),
        ..Default::default()
    }
}

/// A buyer, and a seller's shop with one product in stock.
pub struct Market {
    pub buyer: user::Model,
    pub shop: shop::Model,
    pub category: category::Model,
    pub product: product::Model,
}

pub async fn market(db: &DatabaseConnection) -> Market {
    let buyer = new_user("U-buyer").insert(db).await.unwrap();
    let seller = new_user("U-seller").insert(db).await.unwrap();
    let shop = new_shop(seller.id).insert(db).await.unwrap();
    let category = new_category("ไข่/นม").insert(db).await.unwrap();
    let product = new_product(shop.id, category.id, "ไข่ไก่เบอร์ 2", Some(8))
        .insert(db)
        .await
        .unwrap();
    Market {
        buyer,
        shop,
        category,
        product,
    }
}
