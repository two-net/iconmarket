//! The models against the data rules of SRS §5.2. The `async` tests each start a throwaway
//! PostgreSQL, so they need Docker.

use std::fmt::Debug;

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ActiveEnum, ActiveModelBehavior, ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr,
    EntityTrait, IdenStatic, IntoActiveModel, Iterable, QueryFilter, RuntimeErr, SqlErr,
};
use serde_json::json;
use sqlx::postgres::PgSslMode;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ContainerRequest, ImageExt};

use super::entity::*;
use super::*;
use crate::config::Secret;
use crate::thai::Money;

/// The entities of §5.1, by table name.
const TABLES: [&str; 20] = [
    "ai_call_logs",
    "ai_messages",
    "audit_logs",
    "cart_items",
    "categories",
    "counters",
    "images",
    "order_events",
    "order_items",
    "orders",
    "payment_slips",
    "pending_actions",
    "products",
    "push_logs",
    "ratings",
    "reports",
    "sessions",
    "shops",
    "users",
    "webhook_events",
];

const MONEY_COLUMNS: [(&str, &str); 9] = [
    ("order_items", "line_total"),
    ("order_items", "unit_price"),
    ("orders", "delivery_fee"),
    ("orders", "subtotal"),
    ("orders", "total"),
    ("payment_slips", "ai_amount"),
    ("products", "price"),
    ("shops", "delivery_fee"),
    ("shops", "minimum_order"),
];

/// Column names that hold personal data wherever they appear (DR-10).
const PERSONAL: [&str; 11] = [
    "line_user_id",
    "display_name",
    "picture_url",
    "phone",
    "zone",
    "house_number",
    "landmark_note",
    "address",
    "latitude",
    "longitude",
    "promptpay_id",
];

/// Columns whose names look personal but aren't.
const NOT_PERSONAL: [&str; 1] = ["show_phone"];

#[test]
fn money_is_stored_to_the_satang() {
    for (satang, decimal) in [
        (1, "0.01"),
        (1_250, "12.50"),
        (10_000_000, "100000.00"),
        (19_800_000_000, "198000000.00"),
        (-1_050, "-10.50"),
    ] {
        let money = Money::from_satang(satang);
        assert_eq!(money::to_decimal(money).to_string(), decimal);
        assert_eq!(money::from_decimal(decimal.parse().unwrap()), Some(money));
    }
}

#[test]
fn fractions_of_a_satang_are_refused() {
    assert_eq!(money::from_decimal("0.005".parse().unwrap()), None);
    assert_eq!(
        money::from_decimal("12.500".parse().unwrap()),
        Some(Money::from_satang(1_250))
    );
}

#[test]
fn enums_are_stored_by_their_srs_names() {
    assert_eq!(order::Status::Pending.to_value(), "PENDING");
    assert_eq!(
        order::PaymentStatus::AwaitingConfirmation.to_value(),
        "AWAITING_CONFIRMATION"
    );
    assert_eq!(order::PaymentStatus::RefundDue.to_value(), "REFUND_DUE");
    assert_eq!(order::PaymentMethod::PromptPay.to_value(), "PROMPTPAY");
    assert_eq!(product::Status::Held.to_value(), "HELD");
    assert_eq!(shop::Status::Closed.to_value(), "CLOSED");
}

#[test]
fn random_ids_use_all_their_bits() {
    assert_ne!(random_id(), random_id());
    // A v4 UUID fixes its version bits to 4
    assert!((0..64).any(|_| random_id().get_version_num() != 4));
}

#[test]
fn nothing_is_partitioned() {
    let ddl = format!("{:?}", schema()).to_uppercase();
    assert!(ddl.contains("CREATE TABLE"));
    assert!(!ddl.contains("PARTITION"));
}

/// The columns of `E` named like personal data but missing from `marked`.
fn unmarked<E: EntityTrait>(marked: &[E::Column]) -> Vec<String> {
    let marked: Vec<_> = marked.iter().map(IdenStatic::as_str).collect();
    E::Column::iter()
        .map(|column| column.as_str())
        .filter(|name| PERSONAL.iter().any(|personal| name.contains(personal)))
        .filter(|name| !NOT_PERSONAL.contains(name) && !marked.contains(name))
        .map(|name| format!("{}.{name}", E::default().table_name()))
        .collect()
}

#[test]
fn personal_data_is_marked() {
    let unmarked = [
        unmarked::<ai_call_log::Entity>(&[]),
        unmarked::<ai_message::Entity>(&[]),
        unmarked::<audit_log::Entity>(&[]),
        unmarked::<cart_item::Entity>(&[]),
        unmarked::<category::Entity>(&[]),
        unmarked::<counter::Entity>(&[]),
        unmarked::<image::Entity>(image::Entity::FIELDS),
        unmarked::<order::Entity>(order::Entity::FIELDS),
        unmarked::<order_event::Entity>(&[]),
        unmarked::<order_item::Entity>(&[]),
        unmarked::<payment_slip::Entity>(payment_slip::Entity::FIELDS),
        unmarked::<pending_action::Entity>(&[]),
        unmarked::<product::Entity>(&[]),
        unmarked::<push_log::Entity>(push_log::Entity::FIELDS),
        unmarked::<rating::Entity>(&[]),
        unmarked::<report::Entity>(&[]),
        unmarked::<session::Entity>(&[]),
        unmarked::<shop::Entity>(shop::Entity::FIELDS),
        unmarked::<user::Entity>(user::Entity::FIELDS),
        unmarked::<webhook_event::Entity>(webhook_event::Entity::FIELDS),
    ]
    .concat();
    assert!(
        unmarked.is_empty(),
        "not marked as personal data: {unmarked:?}"
    );

    // Slips, whose columns aren't named like personal data
    assert!(matches!(image::Entity::FIELDS, [image::Column::Data]));
    assert!(
        payment_slip::Entity::FIELDS
            .iter()
            .any(|column| matches!(column, payment_slip::Column::ImageId))
    );
}

/// A fresh database holding the schema, and the container serving it.
async fn database() -> (ContainerAsync<Postgres>, DatabaseConnection) {
    let (container, pool) = start(Postgres::default().into()).await;
    let db = orm(pool);
    schema().apply(&db).await.unwrap();
    (container, db)
}

/// An empty database, and the container serving it.
async fn start(image: ContainerRequest<Postgres>) -> (ContainerAsync<Postgres>, PgPool) {
    let container = image.with_tag("18").start().await.expect(
        "cannot start PostgreSQL; is Docker running at DOCKER_HOST or /var/run/docker.sock?",
    );
    let config = DatabaseConfig {
        host: container.get_host().await.unwrap().to_string(),
        port: container.get_host_port_ipv4(5432).await.unwrap(),
        name: "postgres".to_owned(),
        user: "postgres".to_owned(),
        password: Secret::new("postgres"),
        ssl_mode: PgSslMode::Disable,
        max_connections: 2,
    };
    let pool = connect(&config).await.unwrap();
    (container, pool)
}

/// 22 ต.ค. 2569 14:30 in Bangkok, with microseconds as PostgreSQL keeps them.
fn at() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap() + TimeDelta::microseconds(123_456)
}

fn new_user(line_user_id: &str) -> user::ActiveModel {
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

fn new_shop(owner_id: i64) -> shop::ActiveModel {
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

fn new_category(name: &str) -> category::ActiveModel {
    category::ActiveModel {
        name: Set(name.to_owned()),
        sort_order: Set(1),
        is_active: Set(true),
        ..Default::default()
    }
}

fn new_product(
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

fn new_order(buyer_id: i64, shop_id: i64, number: &str, checkout_key: Uuid) -> order::ActiveModel {
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
struct Market {
    buyer: user::Model,
    shop: shop::Model,
    category: category::Model,
    product: product::Model,
}

async fn market(db: &DatabaseConnection) -> Market {
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

#[track_caller]
fn assert_duplicate<T: Debug>(result: Result<T, DbErr>) {
    let err = result.expect_err("a duplicate was stored");
    assert!(
        matches!(err.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))),
        "{err}"
    );
}

/// The constraint that `err` reports as violated.
fn violated(err: &DbErr) -> Option<&str> {
    match err {
        DbErr::Exec(RuntimeErr::SqlxError(e)) | DbErr::Query(RuntimeErr::SqlxError(e)) => {
            e.as_database_error()?.constraint()
        }
        _ => None,
    }
}

/// Inserts `model` and reads it back from its table, where it must be the only row.
async fn assert_stored<E>(db: &DatabaseConnection, model: E::Model)
where
    E: EntityTrait,
    E::Model: IntoActiveModel<E::ActiveModel> + Clone + PartialEq + Debug,
    E::ActiveModel: ActiveModelBehavior + Send,
{
    model.clone().into_active_model().insert(db).await.unwrap();
    assert_eq!(E::find().one(db).await.unwrap(), Some(model));
}

#[tokio::test]
async fn the_schema_has_every_table_with_exact_money_and_zoned_times() {
    let (_container, db) = database().await;
    let pool = db.get_postgres_connection_pool();

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(tables, TABLES);

    // DR-01: money is numeric(12, 2), and nothing else is numeric
    let numeric: Vec<(String, String, i32, i32)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, numeric_precision::int, numeric_scale::int
         FROM information_schema.columns
         WHERE table_schema = 'public' AND data_type = 'numeric'
         ORDER BY table_name, column_name",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let expected: Vec<_> = MONEY_COLUMNS
        .iter()
        .map(|&(table, column)| (table.to_owned(), column.to_owned(), 12, 2))
        .collect();
    assert_eq!(numeric, expected);

    // Floating point is only for coordinates
    let floats: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_schema = 'public' AND data_type IN ('real', 'double precision')",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(!floats.is_empty());
    assert!(
        floats
            .iter()
            .all(|name| name.ends_with("latitude") || name.ends_with("longitude")),
        "{floats:?}"
    );

    // DR-02: every timestamp has a time zone
    let unzoned: Vec<String> = sqlx::query_scalar(
        "SELECT table_name || '.' || column_name FROM information_schema.columns
         WHERE table_schema = 'public' AND data_type = 'timestamp without time zone'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(unzoned.is_empty(), "{unzoned:?}");
}

/// Every attribute of §5.1, with Thai text, money, times, JSON and bytes.
#[tokio::test]
async fn every_attribute_can_be_stored() {
    let (_container, db) = database().await;
    let at = at();
    let image_id = random_id();

    assert_stored::<user::Entity>(
        &db,
        user::Model {
            id: 1,
            line_user_id: Some("U0123456789abcdef0123456789abcdef".to_owned()),
            display_name: Some("ป้าแดง 🥚".to_owned()),
            picture_url: Some("https://profile.line-scdn.net/0h".to_owned()),
            is_admin: true,
            is_following: true,
            status: user::Status::Suspended,
            consent_version: Some("2026-10".to_owned()),
            consented_at: Some(at),
            zone: Some("ซอย 3".to_owned()),
            house_number: Some("45/2".to_owned()),
            landmark_note: Some("ตรงข้ามวัด".to_owned()),
            phone: Some("0812345678".to_owned()),
            latitude: Some(16.441_9),
            longitude: Some(102.835_9),
            created_at: at,
            updated_at: at,
        },
    )
    .await;
    assert_stored::<shop::Entity>(
        &db,
        shop::Model {
            id: 1,
            owner_id: 1,
            name: "ร้านป้าแดง".to_owned(),
            description: "ไข่ไก่ ผักสวนครัว".to_owned(),
            status: shop::Status::Approved,
            is_open: true,
            opening_hours: Some("ทุกวัน 06:00–10:00".to_owned()),
            offers_pickup: true,
            offers_delivery: true,
            delivery_fee: Money::from_baht(10),
            minimum_order: Some(Money::from_satang(4_950)),
            promptpay_id: Some("0812345678".to_owned()),
            phone: Some("0812345678".to_owned()),
            show_phone: false,
            pickup_address: Some("ซอย 3 บ้านเลขที่ 45/2".to_owned()),
            pickup_latitude: Some(16.441_9),
            pickup_longitude: Some(102.835_9),
            created_at: at,
            updated_at: at,
            deleted_at: Some(at),
        },
    )
    .await;
    assert_stored::<category::Entity>(
        &db,
        category::Model {
            id: 1,
            name: "ขนม/ของหวาน".to_owned(),
            sort_order: 3,
            is_active: false,
        },
    )
    .await;
    assert_stored::<product::Entity>(
        &db,
        product::Model {
            id: 1,
            shop_id: 1,
            category_id: 1,
            name: "ขนมครก".to_owned(),
            description: Some("ขนมครกทำสดวันนี้".to_owned()),
            price: Money::from_satang(10_000_000),
            unit: "กล่อง".to_owned(),
            stock: Some(99_999),
            status: product::Status::Held,
            created_at: at,
            updated_at: at,
            deleted_at: Some(at),
        },
    )
    .await;
    assert_stored::<image::Entity>(
        &db,
        image::Model {
            id: image_id,
            owner_type: image::OwnerType::Slip,
            owner_id: 1,
            position: 2,
            content_type: "image/jpeg".to_owned(),
            width: 1024,
            height: 768,
            size_bytes: 4,
            data: vec![0xFF, 0xD8, 0xFF, 0xD9],
            created_at: at,
        },
    )
    .await;
    assert_stored::<cart_item::Entity>(
        &db,
        cart_item::Model {
            user_id: 1,
            product_id: 1,
            quantity: 99,
            added_at: at,
        },
    )
    .await;
    assert_stored::<order::Entity>(
        &db,
        order::Model {
            id: 1,
            order_number: "IM-20261022-0007".to_owned(),
            checkout_key: random_id(),
            buyer_id: 1,
            shop_id: 1,
            status: order::Status::Cancelled,
            fulfillment: order::Fulfillment::Delivery,
            delivery_zone: Some("ซอย 3".to_owned()),
            delivery_house_number: Some("45/2".to_owned()),
            delivery_landmark_note: Some("ตรงข้ามวัด".to_owned()),
            delivery_latitude: Some(16.441_9),
            delivery_longitude: Some(102.835_9),
            note: Some("ไม่ใส่ถุงค่ะ".to_owned()),
            subtotal: Money::from_baht(220),
            delivery_fee: Money::from_baht(10),
            total: Money::from_baht(230),
            payment_method: order::PaymentMethod::PromptPay,
            payment_status: order::PaymentStatus::RefundDue,
            created_at: at,
            updated_at: at,
            accepted_at: Some(at),
            ready_at: Some(at),
            delivering_at: Some(at),
            completed_at: Some(at),
            rejected_at: Some(at),
            cancelled_at: Some(at),
            expired_at: Some(at),
            cancel_reason: Some("สินค้าหมด".to_owned()),
        },
    )
    .await;
    assert_stored::<order_item::Entity>(
        &db,
        order_item::Model {
            id: 1,
            order_id: 1,
            product_id: 1,
            name: "ไข่ไก่เบอร์ 2".to_owned(),
            unit: "แผง".to_owned(),
            unit_price: Money::from_baht(110),
            quantity: 2,
            line_total: Money::from_baht(220),
        },
    )
    .await;
    assert_stored::<order_event::Entity>(
        &db,
        order_event::Model {
            id: 1,
            order_id: 1,
            from_status: Some(order::Status::Pending),
            to_status: order::Status::Accepted,
            actor_type: order_event::ActorType::Seller,
            actor_id: Some(1),
            reason: Some("อื่นๆ: ร้านปิดเร็ว".to_owned()),
            created_at: at,
        },
    )
    .await;
    assert_stored::<payment_slip::Entity>(
        &db,
        payment_slip::Model {
            id: 1,
            order_id: 1,
            image_id: Some(image_id),
            access_token: random_id(),
            token_expires_at: at,
            ai_amount: Some(Money::from_baht(230)),
            ai_paid_at: Some(at),
            ai_reference: Some("202610221430ABC".to_owned()),
            status: payment_slip::Status::Confirmed,
            created_at: at,
        },
    )
    .await;
    assert_stored::<rating::Entity>(
        &db,
        rating::Model {
            id: 1,
            order_id: 1,
            stars: 5,
            comment: Some("ไข่สดมากค่ะ".to_owned()),
            created_at: at,
        },
    )
    .await;
    assert_stored::<report::Entity>(
        &db,
        report::Model {
            id: 1,
            reporter_id: Some(1),
            target_type: report::TargetType::Product,
            target_id: 1,
            reason: "รูปไม่ตรงกับสินค้า".to_owned(),
            status: report::Status::Resolved,
            resolved_by_id: Some(1),
            resolved_at: Some(at),
            created_at: at,
        },
    )
    .await;
    assert_stored::<session::Entity>(
        &db,
        session::Model {
            user_id: 1,
            flow: "checkout".to_owned(),
            step: "payment".to_owned(),
            data: json!({ "shopId": 1, "note": "ไม่ใส่ถุงค่ะ" }),
            expires_at: at,
        },
    )
    .await;
    assert_stored::<pending_action::Entity>(
        &db,
        pending_action::Model {
            id: random_id(),
            user_id: 1,
            kind: "checkout".to_owned(),
            payload: json!({ "items": [{ "productId": 1, "quantity": 2 }] }),
            expires_at: at,
            consumed_at: Some(at),
            created_at: at,
        },
    )
    .await;
    assert_stored::<ai_message::Entity>(
        &db,
        ai_message::Model {
            id: 1,
            user_id: 1,
            role: ai_message::Role::Tool,
            content: "มีไข่ไก่ขายไหม เอาสัก 2 แผง".to_owned(),
            created_at: at,
        },
    )
    .await;
    assert_stored::<ai_call_log::Entity>(
        &db,
        ai_call_log::Model {
            id: 1,
            user_id: Some(1),
            feature: "assistant".to_owned(),
            provider: "claude".to_owned(),
            model: "a-model".to_owned(),
            input_tokens: 1_200,
            output_tokens: 80,
            latency_ms: 950,
            outcome: "ok".to_owned(),
            created_at: at,
        },
    )
    .await;
    assert_stored::<webhook_event::Entity>(
        &db,
        webhook_event::Model {
            id: 1,
            event_id: "01JABCDEF0123456789ABCDEFG".to_owned(),
            user_key: "U0123456789abcdef0123456789abcdef".to_owned(),
            event_type: "message".to_owned(),
            payload: json!({ "type": "message", "message": { "text": "เมนู" } }),
            received_at: at,
            status: webhook_event::Status::Failed,
            attempts: 3,
            processed_at: Some(at),
            error: Some("timeout".to_owned()),
        },
    )
    .await;
    assert_stored::<push_log::Entity>(
        &db,
        push_log::Model {
            id: 1,
            push_type: "new_order".to_owned(),
            recipient: "U0123456789abcdef0123456789abcdef".to_owned(),
            retry_key: Some(random_id()),
            status: push_log::Status::Failed,
            error: Some("429".to_owned()),
            sent_at: at,
        },
    )
    .await;
    assert_stored::<audit_log::Entity>(
        &db,
        audit_log::Model {
            id: 1,
            actor_id: Some(1),
            action: "approve_shop".to_owned(),
            target_type: "shop".to_owned(),
            target_id: Some(1),
            details: json!({ "reason": "ร้านค้าในหมู่บ้าน" }),
            created_at: at,
        },
    )
    .await;
    assert_stored::<counter::Entity>(
        &db,
        counter::Model {
            key: "order:20261022".to_owned(),
            value: 7,
        },
    )
    .await;
}

/// DR-07
#[tokio::test]
async fn stock_never_goes_negative() {
    let (_container, db) = database().await;
    let Market {
        shop,
        category,
        product,
        ..
    } = market(&db).await;

    for stock in [None, Some(0)] {
        new_product(shop.id, category.id, "ขนมครก", stock)
            .insert(&db)
            .await
            .unwrap();
    }

    let err = new_product(shop.id, category.id, "ขนมครก", Some(-1))
        .insert(&db)
        .await
        .unwrap_err();
    assert_eq!(violated(&err), Some("products_stock_not_negative"), "{err}");

    let mut edit = product.clone().into_active_model();
    edit.stock = Set(Some(-1));
    let err = edit.update(&db).await.unwrap_err();
    assert_eq!(violated(&err), Some("products_stock_not_negative"), "{err}");

    // Reserving more than is left, as an unconditional checkout would
    let err = product::Entity::update_many()
        .col_expr(
            product::Column::Stock,
            Expr::col(product::Column::Stock).sub(9),
        )
        .filter(product::Column::Id.eq(product.id))
        .exec(&db)
        .await
        .unwrap_err();
    assert_eq!(violated(&err), Some("products_stock_not_negative"), "{err}");

    let stored = product::Entity::find_by_id(product.id)
        .one(&db)
        .await
        .unwrap();
    assert_eq!(stored.unwrap().stock, Some(8));
}

/// DR-05 and DR-06
#[tokio::test]
async fn keys_are_enforced() {
    let (_container, db) = database().await;
    let Market {
        buyer,
        shop,
        product,
        ..
    } = market(&db).await;

    assert_duplicate(new_user("U-buyer").insert(&db).await);
    assert_duplicate(new_shop(shop.owner_id).insert(&db).await);

    let line = cart_item::ActiveModel {
        user_id: Set(buyer.id),
        product_id: Set(product.id),
        quantity: Set(2),
        added_at: Set(at()),
    };
    line.clone().insert(&db).await.unwrap();
    assert_duplicate(line.insert(&db).await);

    let checkout_key = random_id();
    let order = new_order(buyer.id, shop.id, "IM-20261022-0001", checkout_key)
        .insert(&db)
        .await
        .unwrap();
    assert_duplicate(
        new_order(buyer.id, shop.id, "IM-20261022-0001", random_id())
            .insert(&db)
            .await,
    );
    assert_duplicate(
        new_order(buyer.id, shop.id, "IM-20261022-0002", checkout_key)
            .insert(&db)
            .await,
    );

    let rating = rating::ActiveModel {
        order_id: Set(order.id),
        stars: Set(5),
        created_at: Set(at()),
        ..Default::default()
    };
    rating.clone().insert(&db).await.unwrap();
    assert_duplicate(rating.insert(&db).await);

    let event = webhook_event::ActiveModel {
        event_id: Set("01JABCDEF0123456789ABCDEFG".to_owned()),
        user_key: Set("U-buyer".to_owned()),
        event_type: Set("follow".to_owned()),
        payload: Set(json!({ "type": "follow" })),
        received_at: Set(at()),
        status: Set(webhook_event::Status::Pending),
        attempts: Set(0),
        ..Default::default()
    };
    event.clone().insert(&db).await.unwrap();
    assert_duplicate(event.insert(&db).await);

    let counter = counter::ActiveModel {
        key: Set("order:20261022".to_owned()),
        value: Set(1),
    };
    counter.clone().insert(&db).await.unwrap();
    assert_duplicate(counter.insert(&db).await);

    // Every reference is checked
    let err = new_product(shop.id + 100, product.category_id, "ขนมครก", None)
        .insert(&db)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err.sql_err(),
            Some(SqlErr::ForeignKeyConstraintViolation(_))
        ),
        "{err}"
    );
}

/// DR-09
#[tokio::test]
async fn thai_text_is_kept_and_compared_without_loss() {
    let (_container, db) = database().await;
    check_encoding(db.get_postgres_connection_pool())
        .await
        .unwrap();
    let Market {
        shop,
        category,
        product,
        ..
    } = market(&db).await;
    new_product(shop.id, category.id, "ขนมครก", None)
        .insert(&db)
        .await
        .unwrap();

    let found = category::Entity::find()
        .filter(category::Column::Name.eq("ไข่/นม"))
        .one(&db)
        .await
        .unwrap();
    assert_eq!(found, Some(category));

    // Keyword search matches inside words, as Thai has no spaces between them (FR-SRCH-04)
    let found = product::Entity::find()
        .filter(product::Column::Name.contains("ไก่"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(found, [product]);
}

/// DR-04
#[tokio::test]
async fn orders_are_never_deleted() {
    let (_container, db) = database().await;
    let Market { buyer, shop, .. } = market(&db).await;
    let order = new_order(buyer.id, shop.id, "IM-20261022-0001", random_id())
        .insert(&db)
        .await
        .unwrap();

    let err = order
        .clone()
        .into_active_model()
        .delete(&db)
        .await
        .unwrap_err();
    assert!(matches!(err, DbErr::Custom(_)), "{err}");
    let stored = order::Entity::find_by_id(order.id).one(&db).await.unwrap();
    assert_eq!(stored, Some(order));
}

/// DR-09
#[tokio::test]
async fn a_database_without_utf8_is_refused() {
    let image = Postgres::default().with_env_var("POSTGRES_INITDB_ARGS", "-E SQL_ASCII --locale=C");
    let (_container, pool) = start(image).await;
    let err = check_encoding(&pool).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "the database encoding is SQL_ASCII, but Thai text needs UTF8 (DR-09)"
    );
}
