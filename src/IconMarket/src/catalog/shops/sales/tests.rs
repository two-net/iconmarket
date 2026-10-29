//! "ยอดขาย" (FR-SHOP-11): the figures against a seeded shop, its orders on either side of each
//! period's Bangkok midnight, beside another seller's shop; the card that shows them; and who
//! may see it. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Iterable, TransactionTrait};
use serde_json::Value;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::line::limits;
use crate::notifications::templates::order_status_name;
use crate::persistence::entity::order::{PaymentStatus, Status};
use crate::persistence::entity::{product, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

/// 22 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    testing::at()
}

fn utc(month: u32, day: u32, hour: u32) -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, month, day, hour, 0, 0).unwrap()
}

/// 22 ต.ค. 00:00 in Bangkok, when today began
fn today() -> DateTimeUtc {
    utc(10, 21, 17)
}

/// 16 ต.ค. 00:00 in Bangkok, when the last 7 days began
fn week() -> DateTimeUtc {
    utc(10, 15, 17)
}

/// 23 ก.ย. 00:00 in Bangkok, when the last 30 days began
fn month() -> DateTimeUtc {
    utc(9, 22, 17)
}

/// An instant before `at`
fn just_before(at: DateTimeUtc) -> DateTimeUtc {
    at - chrono::TimeDelta::seconds(1)
}

/// The products of the seller's shop, as [`Setup::seed`] sells them
#[derive(Clone, Copy)]
enum Sold {
    /// Sold as ไข่ไก่, then as ไข่ไก่ เบอร์ 0, and now named ไข่ไก่สด
    Eggs,
    Greens,
    Rice,
    Chillies,
    Bananas,
    /// Deleted since
    Mackerel,
    Lemongrass,
}

impl Sold {
    const ALL: [Self; 7] = [
        Self::Eggs,
        Self::Greens,
        Self::Rice,
        Self::Chillies,
        Self::Bananas,
        Self::Mackerel,
        Self::Lemongrass,
    ];

    /// Its name now, its unit and its price in baht
    fn product(self) -> (&'static str, &'static str, i64) {
        match self {
            Self::Eggs => ("ไข่ไก่สด", "แผง", 120),
            Self::Greens => ("ผักบุ้ง", "กำ", 10),
            Self::Rice => ("ข้าวสาร", "กก.", 30),
            Self::Chillies => ("พริก", "ขีด", 15),
            Self::Bananas => ("กล้วย", "หวี", 40),
            Self::Mackerel => ("ปลาทู", "เข่ง", 200),
            Self::Lemongrass => ("ตะไคร้", "มัด", 25),
        }
    }
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    config: Config,
    buyer: i64,
    /// The seller whose sales are seeded, and their shop
    seller: i64,
    shop: i64,
    /// Another seller, with an order of their own
    other: i64,
    other_shop: i64,
    /// The seller's products, as [`Sold::ALL`] lists them
    products: Vec<i64>,
}

/// A buyer, the seller with their products and no orders, and another seller.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let buyer = testing::new_user("U-buyer").insert(&db).await.unwrap().id;
    let seller = testing::new_user("U-seller").insert(&db).await.unwrap().id;
    let shop = testing::new_shop(seller).insert(&db).await.unwrap().id;
    let other = testing::new_user("U-other").insert(&db).await.unwrap().id;
    let other_shop = shop::ActiveModel {
        name: Set("ร้านลุงชม".to_owned()),
        ..testing::new_shop(other)
    };
    let other_shop = other_shop.insert(&db).await.unwrap().id;
    let category = testing::new_category("ของสด").insert(&db).await.unwrap().id;
    let mut products = Vec::new();
    for sold in Sold::ALL {
        let (name, unit, baht) = sold.product();
        let deleted = matches!(sold, Sold::Mackerel);
        let product = product::ActiveModel {
            unit: Set(unit.to_owned()),
            price: Set(Money::from_baht(baht)),
            status: Set(if deleted {
                product::Status::Deleted
            } else {
                product::Status::Active
            }),
            deleted_at: Set(deleted.then(t0)),
            ..testing::new_product(shop, category, name, None)
        };
        products.push(product.insert(&db).await.unwrap().id);
    }
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        buyer,
        seller,
        shop,
        other,
        other_shop,
        products,
    }
}

/// An order's lines of the seller's products: what was sold, under what name, how many, and at
/// what price in baht
type Lines<'a> = &'a [(Sold, &'a str, i32, i64)];

/// A line of an order as it is stored: the product's ID, its name and unit, how many, and the
/// price in baht
type Line<'a> = (i64, &'a str, &'a str, i32, i64);

impl Setup {
    /// User `user_id` taps "ยอดขาย" at `t0()`, as routing hands it over.
    async fn taps(&self, user_id: i64) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let reply = tapped(&turn, &self.config).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        reply
    }

    async fn gather(&self, shop_id: i64) -> Sales {
        gather(&self.db, self.config.app.time_zone, shop_id, t0())
            .await
            .unwrap()
    }

    async fn shop(&self, shop_id: i64) -> shop::Model {
        shop::Entity::find_by_id(shop_id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// `lines` of the seller's products as they are stored.
    fn lines<'a>(&self, lines: Lines<'a>) -> Vec<Line<'a>> {
        lines
            .iter()
            .map(|&(sold, name, quantity, baht)| {
                let (_, unit, _) = sold.product();
                (self.products[sold as usize], name, unit, quantity, baht)
            })
            .collect()
    }

    /// Order `n` at shop `shop_id`, placed at `placed` and now `status` and `payment`, of
    /// `items`.
    async fn order(
        &self,
        n: u128,
        shop_id: i64,
        placed: DateTimeUtc,
        status: Status,
        payment: PaymentStatus,
        items: &[Line<'_>],
    ) {
        let total: i64 = items
            .iter()
            .map(|&(.., quantity, baht)| i64::from(quantity) * baht)
            .sum();
        let number = format!("IM-20261022-{n:04}");
        let order = order::ActiveModel {
            status: Set(status),
            payment_status: Set(payment),
            subtotal: Set(Money::from_baht(total)),
            total: Set(Money::from_baht(total)),
            created_at: Set(placed),
            updated_at: Set(placed),
            ..testing::new_order(self.buyer, shop_id, &number, Uuid::from_u128(n))
        };
        let order = order.insert(&self.db).await.unwrap();
        for &(product_id, name, unit, quantity, baht) in items {
            order_item::ActiveModel {
                order_id: Set(order.id),
                product_id: Set(product_id),
                name: Set(name.to_owned()),
                unit: Set(unit.to_owned()),
                unit_price: Set(Money::from_baht(baht)),
                quantity: Set(quantity),
                line_total: Set(Money::from_baht(i64::from(quantity) * baht)),
                ..Default::default()
            }
            .insert(&self.db)
            .await
            .unwrap();
        }
    }

    /// The seller's orders, placed one after another, on either side of each period's start,
    /// of every status the figures tell apart; and an order at the other shop, which counts
    /// for neither, of its product it gives the ID of. [`shop_sales`] is what the seller's
    /// hold.
    async fn seed(&self) -> i64 {
        use PaymentStatus::*;
        use Sold::*;
        use Status::*;
        let shop = self.shop;
        let orders: [(DateTimeUtc, Status, PaymentStatus, Lines<'_>); 11] = [
            // Before the last 30 days: in no period
            (
                just_before(month()),
                Completed,
                Paid,
                &[(Rice, "ข้าวสาร", 5, 30)],
            ),
            (month(), Completed, Unpaid, &[(Eggs, "ไข่ไก่", 2, 110)]),
            (
                utc(10, 1, 3),
                Rejected,
                Unpaid,
                &[(Mackerel, "ปลาทู", 1, 200)],
            ),
            (
                just_before(week()),
                Completed,
                AwaitingConfirmation,
                &[(Greens, "ผักบุ้ง", 10, 10), (Chillies, "พริก", 2, 15)],
            ),
            (
                week(),
                Completed,
                Paid,
                &[(Eggs, "ไข่ไก่ เบอร์ 0", 1, 120), (Bananas, "กล้วย", 3, 40)],
            ),
            // Paid, then cancelled: to be refunded, but not a completed order unpaid
            (
                utc(10, 18, 3),
                Cancelled,
                RefundDue,
                &[(Lemongrass, "ตะไคร้", 4, 25)],
            ),
            (
                just_before(today()),
                Completed,
                Unpaid,
                &[(Chillies, "พริก", 4, 15), (Lemongrass, "ตะไคร้", 2, 25)],
            ),
            (
                today(),
                Completed,
                Paid,
                &[(Greens, "ผักบุ้ง", 5, 10), (Mackerel, "ปลาทู", 1, 200)],
            ),
            (
                utc(10, 22, 3),
                Delivering,
                Unpaid,
                &[(Rice, "ข้าวสาร", 2, 30)],
            ),
            (
                utc(10, 22, 5),
                Completed,
                AwaitingConfirmation,
                &[(Bananas, "กล้วย", 1, 40)],
            ),
            // Not yet handed over, so neither sold nor unpaid
            (
                utc(10, 22, 7),
                Pending,
                Unpaid,
                &[(Eggs, "ไข่ไก่ เบอร์ 0", 1, 120)],
            ),
        ];
        for (n, (placed, status, payment, lines)) in (1..).zip(orders) {
            let lines = self.lines(lines);
            self.order(n, shop, placed, status, payment, &lines).await;
        }

        let category = testing::new_category("น้ำผึ้ง")
            .insert(&self.db)
            .await
            .unwrap();
        let honey = testing::new_product(self.other_shop, category.id, "น้ำผึ้งป่า", None);
        let honey = honey.insert(&self.db).await.unwrap().id;
        let lines = [(honey, "น้ำผึ้งป่า", "ขวด", 1, 999)];
        let (shop, placed) = (self.other_shop, utc(10, 22, 4));
        self.order(99, shop, placed, Completed, Unpaid, &lines)
            .await;
        honey
    }

    /// Product `sold`, `quantity` of it sold for `baht`, under the name `name`
    fn sold(&self, sold: Sold, name: &str, quantity: u64, baht: i64) -> TopProduct {
        let (_, unit, _) = sold.product();
        TopProduct {
            product_id: self.products[sold as usize],
            name: name.to_owned(),
            unit: unit.to_owned(),
            quantity,
            value: Money::from_baht(baht),
        }
    }
}

/// The orders of a period: how many have each status, in the order of Table 4.6-A, and the
/// value of those completed, in baht.
fn orders(days: u64, counts: [u64; 8], baht: i64) -> Orders {
    Orders {
        days,
        by_status: Status::iter().zip(counts).collect(),
        completed_value: Money::from_baht(baht),
    }
}

/// What [`Setup::seed`] holds for the seller's shop, worked out by hand.
fn shop_sales(s: &Setup) -> Sales {
    use Sold::*;
    Sales {
        periods: [
            Period {
                // Completed today: ฿250 and ฿40, the second of them not yet confirmed paid
                orders: orders(1, [1, 0, 0, 1, 2, 0, 0, 0], 290),
                unpaid: 1,
                unpaid_value: Money::from_baht(40),
                top: vec![
                    s.sold(Mackerel, "ปลาทู", 1, 200),
                    s.sold(Greens, "ผักบุ้ง", 5, 50),
                    s.sold(Bananas, "กล้วย", 1, 40),
                ],
            },
            Period {
                orders: orders(7, [1, 0, 0, 1, 4, 0, 1, 0], 640),
                unpaid: 2,
                unpaid_value: Money::from_baht(150),
                // ผักบุ้ง and ตะไคร้ both sold ฿50, so the 5 กำ come before the 2 มัด, which
                // leaves ตะไคร้ out
                top: vec![
                    s.sold(Mackerel, "ปลาทู", 1, 200),
                    s.sold(Bananas, "กล้วย", 4, 160),
                    s.sold(Eggs, "ไข่ไก่ เบอร์ 0", 1, 120),
                    s.sold(Chillies, "พริก", 4, 60),
                    s.sold(Greens, "ผักบุ้ง", 5, 50),
                ],
            },
            Period {
                orders: orders(30, [1, 0, 0, 1, 6, 1, 1, 0], 990),
                unpaid: 4,
                unpaid_value: Money::from_baht(500),
                // The eggs as last sold, ไข่ไก่ เบอร์ 0, neither ไข่ไก่ nor ไข่ไก่สด
                top: vec![
                    s.sold(Eggs, "ไข่ไก่ เบอร์ 0", 3, 340),
                    s.sold(Mackerel, "ปลาทู", 1, 200),
                    s.sold(Bananas, "กล้วย", 4, 160),
                    s.sold(Greens, "ผักบุ้ง", 15, 150),
                    s.sold(Chillies, "พริก", 6, 90),
                ],
            },
        ],
    }
}

/// The texts of `message`'s card, in order.
fn texts(message: &Message) -> Vec<String> {
    fn walk(value: &Value, texts: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                if fields.get("type") == Some(&Value::from("text"))
                    && let Some(Value::String(text)) = fields.get("text")
                {
                    texts.push(text.clone());
                }
                fields.values().for_each(|value| walk(value, texts));
            }
            Value::Array(values) => values.iter().for_each(|value| walk(value, texts)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    walk(
        &serde_json::to_value(message).unwrap()["contents"],
        &mut texts,
    );
    texts
}

/// Whether `texts` hold `row` in a row.
fn has_row(texts: &[String], row: &[&str]) -> bool {
    texts.windows(row.len()).any(|window| window == row)
}

/// FR-SHOP-11 acceptance: every figure is the database's, counted by the Bangkok day, and only
/// the shop's own orders count.
#[tokio::test]
async fn the_figures_match_the_database() {
    let s = setup().await;
    let none = |days| Period {
        orders: orders(days, [0; 8], 0),
        unpaid: 0,
        unpaid_value: Money::ZERO,
        top: Vec::new(),
    };
    let nothing = Sales {
        periods: [none(1), none(7), none(30)],
    };
    assert_eq!(s.gather(s.shop).await, nothing);

    s.seed().await;
    assert_eq!(s.gather(s.shop).await, shop_sales(&s));
}

/// FR-SHOP-11 acceptance, NFR-SEC-02: a seller sees only their own shop's figures, as the other
/// shop's sales show none of the seller's.
#[tokio::test]
async fn a_seller_sees_only_their_own_shop() {
    let s = setup().await;
    let honey_id = s.seed().await;
    let honey = |days| Period {
        orders: orders(days, [0, 0, 0, 0, 1, 0, 0, 0], 999),
        unpaid: 1,
        unpaid_value: Money::from_baht(999),
        top: vec![TopProduct {
            product_id: honey_id,
            name: "น้ำผึ้งป่า".to_owned(),
            unit: "ขวด".to_owned(),
            quantity: 1,
            value: Money::from_baht(999),
        }],
    };
    let others = Sales {
        periods: [honey(1), honey(7), honey(30)],
    };
    assert_eq!(s.gather(s.other_shop).await, others);

    let reply = s.taps(s.other).await;
    let shop = s.shop(s.other_shop).await;
    assert_eq!(reply, [card(&s.templates, &shop, &others, t0())]);
    let texts = texts(&reply[0]);
    assert!(
        has_row(&texts, &["1. น้ำผึ้งป่า", "1 ขวด", "฿999"]),
        "{texts:?}"
    );
    assert!(!texts.iter().any(|text| text.contains("ผักบุ้ง")), "{texts:?}");
}

/// "ยอดขาย" is one card: the shop, a table of orders by status and period with the value of
/// those completed and of those unpaid, each period's best sellers, and buttons to see it
/// again and to see the orders unpaid.
#[tokio::test]
async fn the_card_shows_the_figures() {
    let s = setup().await;
    let reply = s.taps(s.seller).await;
    let empty = texts(&reply[0]);
    let nothing_sold = [
        "สินค้าขายดี",
        "วันนี้",
        "ยังไม่มี",
        "7 วัน",
        "ยังไม่มี",
        "30 วัน",
        "ยังไม่มี",
    ];
    assert!(has_row(&empty, &nothing_sold), "{empty:?}");

    s.seed().await;
    let reply = s.taps(s.seller).await;
    limits::check(&reply).unwrap();
    let [card] = &reply[..] else {
        panic!("one card: {reply:?}")
    };
    let shop = s.shop(s.shop).await;
    assert_eq!(
        card,
        &super::card(&s.templates, &shop, &shop_sales(&s), t0())
    );
    let texts = texts(card);
    for row in [
        &["ยอดขาย", "ร้านป้าแดง", "ณ 22 ต.ค. 2569 14:30"][..],
        &["สถานะ", "วันนี้", "7 วัน", "30 วัน"],
        &[order_status_name(Status::Pending), "1", "1", "1"],
        &[order_status_name(Status::Completed), "2", "4", "6"],
        &[order_status_name(Status::Expired), "0", "0", "0"],
        &["รวม", "4", "7", "10"],
        &["ยอดที่สำเร็จ", "฿290", "฿640", "฿990"],
        &["ค้างชำระ", "1", "2", "4"],
        &["ยอดค้างชำระ", "฿40", "฿150", "฿500"],
        &[
            "วันนี้",
            "1. ปลาทู",
            "1 เข่ง",
            "฿200",
            "2. ผักบุ้ง",
            "5 กำ",
            "฿50",
            "3. กล้วย",
            "1 หวี",
            "฿40",
            "7 วัน",
        ],
        &[
            "5. ผักบุ้ง",
            "5 กำ",
            "฿50",
            "30 วัน",
            "1. ไข่ไก่ เบอร์ 0",
            "3 แผง",
            "฿340",
        ],
        &["4. ผักบุ้ง", "15 กำ", "฿150"],
    ] {
        assert!(has_row(&texts, row), "no {row:?} in {texts:?}");
    }

    let json = serde_json::to_string(card).unwrap();
    assert!(json.contains(r#""data":"sales""#), "{json}");
    assert!(json.contains(labels::REFRESH.as_str()), "{json}");
    let unpaid = format!(r#""data":"{}""#, lists::unpaid_list());
    assert!(json.contains(&unpaid), "{json}");
    assert!(json.contains(labels::FILTER_UNPAID.as_str()), "{json}");
}

/// FR-SHOP-10, NFR-SEC-02: only the owner of an approved shop, or of one held for review, sees
/// its sales; not a buyer, not while their shop is suspended, and not while they are.
#[tokio::test]
async fn only_the_shops_owner_sees_its_sales() {
    let s = setup().await;
    s.seed().await;
    assert_eq!(
        s.taps(s.buyer).await,
        [s.templates.denied(Denied::NotAllowed)]
    );

    let set_status = async |status, reason: Option<&str>| {
        shop::ActiveModel {
            id: Set(s.shop),
            status: Set(status),
            suspension_reason: Set(reason.map(str::to_owned)),
            ..Default::default()
        }
        .update(&s.db)
        .await
        .unwrap()
    };
    let held = set_status(shop::Status::Held, None).await;
    assert_eq!(
        s.taps(s.seller).await,
        [card(&s.templates, &held, &shop_sales(&s), t0())]
    );
    let reason = "ขายของต้องห้าม";
    set_status(shop::Status::Suspended, Some(reason)).await;
    assert_eq!(
        s.taps(s.seller).await,
        [s.templates.refused(Denied::ShopSuspended, Some(reason))]
    );

    set_status(shop::Status::Approved, None).await;
    let reason = "ทดสอบการระงับ";
    user::ActiveModel {
        id: Set(s.seller),
        status: Set(user::Status::Suspended),
        suspension_reason: Set(Some(reason.to_owned())),
        ..Default::default()
    }
    .update(&s.db)
    .await
    .unwrap();
    assert_eq!(
        s.taps(s.seller).await,
        [s.templates.refused(Denied::Suspended, Some(reason))]
    );
}
