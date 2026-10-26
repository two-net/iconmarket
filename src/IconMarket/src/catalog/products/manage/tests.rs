//! "สินค้าของฉัน" (FR-PROD-05, FR-PROD-06): the shop's products paged, sold-out ones first; a
//! sold-out product restocked with a tap and the quantity; each part changed alone and saved
//! once answered, leaving orders as they were (FR-PROD-08) and stock taken meanwhile alone;
//! photos added or replaced; hiding and showing at a tap; deleting confirmed, and soft (DR-04);
//! changed text and new photos moderated (FR-PROD-09); and only the owner of an approved shop
//! doing any of it. Each event is handled in a
//! transaction committed once it is handled. The `async` tests each start a throwaway
//! PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder};
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::ai::testing as scripted;
use crate::catalog::products::{Availability, MAX_STOCK};
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::dispatcher::pending;
use crate::line::flex::{Bubble, Component, FlexContainer, Hero};
use crate::line::testing::{self as fake, Fake};
use crate::line::{Body, limits};
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{order, order_item, push_log, report, session, shop, user};
use crate::persistence::{orm, random_id, testing};
use crate::thai::{Money, NumberError};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const ADMIN: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../../ai/provider/fixtures/media/slip.jpg");

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    /// The pool [`Setup::db`] is on, for the AI's daily limits
    pool: PgPool,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    /// Off, unless a test scripts it
    ai: ai::Gateway,
    templates: Templates,
    /// The owner of [`Setup::shop`]
    seller: i64,
    /// Approved and open
    shop: i64,
    /// The owner of another approved shop
    other_seller: i64,
    other_shop: i64,
    /// A user without a shop
    buyer: i64,
    /// The active categories, in order: ไข่/นม and ขนม/ของหวาน. ผลไม้ follows, inactive.
    eggs: i64,
    sweets: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool.clone());
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let shop = testing::new_shop(seller).insert(&db).await.unwrap().id;
    let other_seller = testing::new_user(OTHER_SELLER)
        .insert(&db)
        .await
        .unwrap()
        .id;
    let other_shop = shop::ActiveModel {
        name: Set("ร้านลุงชม".to_owned()),
        ..testing::new_shop(other_seller)
    };
    let other_shop = other_shop.insert(&db).await.unwrap().id;
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    let category = async |name: &str, sort_order: i32, is_active: bool| {
        let category = category::ActiveModel {
            sort_order: Set(sort_order),
            is_active: Set(is_active),
            ..testing::new_category(name)
        };
        category.insert(&db).await.unwrap().id
    };
    let eggs = category("ไข่/นม", 1, true).await;
    let sweets = category("ขนม/ของหวาน", 2, true).await;
    category("ผลไม้", 3, false).await;
    let config = config::sample();
    Setup {
        _container: container,
        pool,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        ai: ai::testing::off(),
        seller,
        shop,
        other_seller,
        other_shop,
        buyer,
        eggs,
        sweets,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `now` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh and what it
    /// notifies queued.
    async fn handle_at(
        &self,
        user_id: i64,
        now: DateTimeUtc,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, now)
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages, outbox, ..
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.config.notification, now)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        messages
    }

    /// User `user_id` taps a button sending `data` at `now`, as routing hands it over.
    async fn taps_at(&self, user_id: i64, data: &str, now: DateTimeUtc) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle_at(user_id, now, async |turn| {
            if postback.action() == pending::CONFIRM {
                pending::tapped(turn, &postback).await
            } else {
                tapped(turn, &self.config, &postback).await
            }
        })
        .await
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.taps_at(user_id, data, t0()).await
    }

    /// The seller taps the button changing `part` of product `id`.
    async fn changes(&self, id: i64, part: &str) -> Vec<Message> {
        self.taps(self.seller, &format!("my_products.part?id={id}&p={part}"))
            .await
    }

    /// User `user_id` answers the part they are changing with `input`; `None` if the step
    /// doesn't take it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let line = self.line.client();
        let mut answered = None;
        self.handle_at(user_id, t0(), async |turn| {
            let session = turn.flow.as_ref().and_then(|flow| flow.read::<EditFlow>());
            let session = session.expect("changing a product");
            answered = answer(turn, &self.config, &self.ai, &line, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    /// The seller types `text` as the answer to the part they are changing.
    async fn types(&self, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(self.seller, input).await.expect("an answer")
    }

    /// The seller sends a photo, which LINE serves as `bytes`.
    async fn sends_photo(&self, message_id: &str, bytes: &[u8]) -> Option<Vec<Message>> {
        let path = format!("/v2/bot/message/{message_id}/content");
        self.line
            .script(&path, [fake::Answer::bytes("image/jpeg", bytes)]);
        let input = Input::Image {
            message_id: message_id.to_owned(),
        };
        self.sends(self.seller, input).await
    }

    /// Lists a product called `name` with `stock` in `shop`, ฿110 a แผง of eggs.
    async fn list_in(&self, shop: i64, name: &str, stock: Option<i32>) -> i64 {
        let product = testing::new_product(shop, self.eggs, name, stock);
        product.insert(&self.db).await.unwrap().id
    }

    /// Lists a product of the seller's.
    async fn product(&self, name: &str, stock: Option<i32>) -> i64 {
        self.list_in(self.shop, name, stock).await
    }

    async fn get(&self, id: i64) -> product::Model {
        product::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Sets product `id`'s `status`, as an admin's decision or a deletion would.
    async fn set_status(&self, id: i64, status: product::Status) {
        product::Entity::update_many()
            .col_expr(product::Column::Status, Expr::value(status))
            .filter(product::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    /// An admin, whom the admins' alerts go to.
    async fn admin(&self) -> i64 {
        let admin = user::ActiveModel {
            is_admin: Set(true),
            ..testing::new_user(ADMIN)
        };
        admin.insert(&self.db).await.unwrap().id
    }

    /// Every report, as (reporter, target type, target, reason).
    async fn reports(&self) -> Vec<(Option<i64>, report::TargetType, i64, String)> {
        report::Entity::find()
            .order_by_asc(report::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.reporter_id, row.target_type, row.target_id, row.reason))
            .collect()
    }

    /// The types of every push queued, oldest first.
    async fn pushes(&self) -> Vec<String> {
        push_log::Entity::find()
            .order_by_asc(push_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.push_type)
            .collect()
    }

    /// Gives product `id` another photo, as listing it with one would.
    async fn add_photo(&self, id: i64) -> Uuid {
        let processed = images::prepare(PHOTO.to_vec(), &self.config.image)
            .await
            .unwrap();
        images::store(
            &self.db,
            Owner::Product(id),
            processed,
            &self.config.image,
            t0(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    /// Every image, as (owner type, owner, position), by position.
    async fn images(&self) -> Vec<(OwnerType, i64, i16, Uuid)> {
        image::Entity::find()
            .order_by_asc(image::Column::OwnerType)
            .order_by_asc(image::Column::Position)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.owner_type, row.owner_id, row.position, row.id))
            .collect()
    }

    async fn shop(&self) -> shop::Model {
        shop::Entity::find_by_id(self.shop)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn flow(&self, user_id: i64) -> Option<String> {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .map(|row| row.flow)
    }

    fn photo_url(&self, id: Uuid) -> String {
        images::product_image_url(&self.config.app.base_url, id).to_string()
    }

    /// "สินค้าของฉัน"'s first page, as the seller sees it now.
    async fn list(&self) -> Message {
        let mut list = self.taps(self.seller, "my_products").await;
        assert_eq!(list.len(), 1);
        list.remove(0)
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The labels of `message`'s Quick Reply buttons.
fn quick_replies(message: &Message) -> Vec<&str> {
    let Some(quick_reply) = &message.quick_reply else {
        return Vec::new();
    };
    quick_reply
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
}

/// The postback data of `message`'s Quick Reply buttons; `None` for those that aren't.
fn quick_reply_data(message: &Message) -> Vec<Option<&str>> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| match &item.action {
            Action::Postback { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect()
}

/// The cards of `message`, a carousel or a single card.
fn cards(message: &Message) -> Vec<&Bubble> {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } => carousel.contents.iter().collect(),
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => vec![bubble],
        body => panic!("not a card: {body:?}"),
    }
}

/// The one card of `message`.
fn card(message: &Message) -> &Bubble {
    let [card] = cards(message)[..] else {
        panic!("not one card: {message:?}");
    };
    card
}

/// The texts of `card`'s body, in order, the rows' labels and values among them: its name,
/// price and state first.
fn texts(card: &Bubble) -> Vec<String> {
    fn texts(component: &Component, out: &mut Vec<String>) {
        match component {
            Component::Text(text) => out.push(text.text.clone()),
            Component::Box(flex_box) => {
                for component in &flex_box.contents {
                    texts(component, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for component in &card.body.as_ref().unwrap().contents {
        texts(component, &mut out);
    }
    out
}

/// The value of the row labelled `label` in `card`.
fn row(card: &Bubble, label: &str) -> String {
    let texts = texts(card);
    let at = texts.iter().position(|text| text == label).expect(label);
    texts[at + 1].clone()
}

/// The label and postback data of each button of `card`, row after row.
fn buttons(card: &Bubble) -> Vec<(String, String)> {
    fn buttons(component: &Component, out: &mut Vec<(String, String)>) {
        match component {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => {
                    out.push((label.as_str().to_owned(), data.clone()));
                }
                action => panic!("not a postback: {action:?}"),
            },
            Component::Box(flex_box) => {
                for component in &flex_box.contents {
                    buttons(component, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for component in &card.footer.as_ref().expect("buttons").contents {
        buttons(component, &mut out);
    }
    out
}

fn labels_of(card: &Bubble) -> Vec<String> {
    buttons(card).into_iter().map(|(label, _)| label).collect()
}

/// The link of the photo across the top of `card`.
fn hero(card: &Bubble) -> Option<&str> {
    match &card.hero {
        Some(Hero::Image(image)) => Some(&image.url),
        _ => None,
    }
}

/// The buttons of a listed product that isn't sold out.
const LISTED: [&str; 6] = ["แก้จำนวน", "แก้ราคา", "เปลี่ยนรูป", "แก้ไข", "ซ่อนสินค้า", "ลบ"];

/// FR-PROD-06, acceptance: a sold-out product says "หมด" and can't be ordered; "เติมของ", its
/// first button, then the quantity make it orderable again.
#[tokio::test]
async fn a_sold_out_product_is_restocked_with_a_tap_and_the_quantity() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่เบอร์ 2", Some(0)).await;
    let shop = s.shop().await;
    let sold_out = Availability::of(&s.get(eggs).await, &shop);
    assert_eq!(sold_out, Availability::SoldOut);
    assert!(!sold_out.orderable());

    let list = s.list().await;
    let listed = card(&list);
    assert_eq!(texts(listed)[..3], ["ไข่ไก่เบอร์ 2", "฿110 / แผง", "หมด"]);
    assert_eq!(row(listed, "จำนวน"), "0 แผง (ลูกค้าจะเห็นว่าหมด)");
    let (label, restock) = buttons(listed).remove(0);
    assert_eq!(label, "เติมของ");
    assert_eq!(restock, format!("my_products.part?id={eggs}&p=stock"));

    // One tap…
    let ask = s.taps(s.seller, &restock).await;
    assert_eq!(ask.len(), 2);
    assert_eq!(ask[0], t.editing_product("ไข่ไก่เบอร์ 2"));
    assert!(
        text(&ask[1]).starts_with("ตอนนี้มีขายกี่แผงคะ"),
        "{}",
        text(&ask[1])
    );
    assert!(
        text(&ask[1]).ends_with("(ตอนนี้: 0 แผง (ลูกค้าจะเห็นว่าหมด))"),
        "{}",
        text(&ask[1])
    );
    assert_eq!(
        quick_replies(&ask[1]),
        ["ไม่จำกัด / ทำตามสั่ง", "ใช้ของเดิม", "ยกเลิก"]
    );
    assert_eq!(s.flow(s.seller).await.as_deref(), Some(EditFlow::NAME));

    // …and the quantity
    let done = s.types("30").await;
    assert_eq!(done.len(), 2);
    assert_eq!(done[0], t.product_stock_saved(Some(30), "แผง", true));
    assert_eq!(text(&done[0]), "เติมของแล้วค่ะ ลูกค้าสั่งสินค้านี้ได้อีกครั้งค่ะ");
    let now = card(&done[1]);
    assert_eq!(texts(now)[2], "ลงขายอยู่");
    assert_eq!(row(now, "จำนวน"), "30 แผง");
    assert_eq!(labels_of(now), LISTED);
    assert_eq!(quick_replies(&done[1]), ["สินค้าของฉัน", "เมนู"]);
    assert_eq!(s.flow(s.seller).await, None);

    let restocked = s.get(eggs).await;
    assert_eq!((restocked.stock, restocked.updated_at), (Some(30), t0()));
    let orderable = Availability::of(&restocked, &shop);
    assert_eq!(orderable, Availability::Left(30));
    assert!(orderable.orderable());
}

/// FR-PROD-08, DR-03, acceptance: a new price is for new orders; an order placed before keeps
/// the price, and the total, it was placed at.
#[tokio::test]
async fn a_new_price_leaves_orders_as_they_were() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่เบอร์ 2", Some(30)).await;
    let number = "IM-20261025-0001";
    let order = testing::new_order(s.buyer, s.shop, number, random_id());
    let order = order.insert(&s.db).await.unwrap();
    let item = order_item::ActiveModel {
        order_id: Set(order.id),
        product_id: Set(eggs),
        name: Set("ไข่ไก่เบอร์ 2".to_owned()),
        unit: Set("แผง".to_owned()),
        unit_price: Set(Money::from_baht(110)),
        quantity: Set(2),
        line_total: Set(Money::from_baht(220)),
        ..Default::default()
    };
    let item = item.insert(&s.db).await.unwrap();

    let ask = s.changes(eggs, "price").await;
    assert_eq!(ask[0], t.editing_product("ไข่ไก่เบอร์ 2"));
    assert!(text(&ask[1]).ends_with("(ตอนนี้: ฿110)"), "{}", text(&ask[1]));
    let done = s.types("๑๒๐ บาท").await;
    assert_eq!(done[0], t.product_price_saved(Money::from_baht(120), "แผง"));
    assert!(text(&done[0]).contains("ออเดอร์ที่สั่งไว้แล้วยังใช้ราคาเดิม"));
    assert_eq!(texts(card(&done[1]))[1], "฿120 / แผง");

    assert_eq!(s.get(eggs).await.price, Money::from_baht(120));
    let kept = order_item::Entity::find_by_id(item.id)
        .one(&s.db)
        .await
        .unwrap();
    assert_eq!(kept, Some(item));
    let kept = order::Entity::find_by_id(order.id)
        .one(&s.db)
        .await
        .unwrap();
    assert_eq!(kept, Some(order));
}

/// FR-PROD-05, #32: only the part answered is written, so stock that orders took while the
/// seller was typing a price stays taken.
#[tokio::test]
async fn only_the_part_answered_is_written() {
    let s = setup().await;
    let eggs = s.product("ไข่ไก่เบอร์ 2", Some(5)).await;
    s.changes(eggs, "price").await;
    // A checkout takes 2 meanwhile
    product::Entity::update_many()
        .col_expr(product::Column::Stock, Expr::value(3))
        .filter(product::Column::Id.eq(eggs))
        .exec(&s.db)
        .await
        .unwrap();
    s.types("120").await;
    let product = s.get(eggs).await;
    assert_eq!(
        (product.price, product.stock),
        (Money::from_baht(120), Some(3))
    );
}

/// FR-PROD-03, FR-PROD-05: the stock is what is left for new orders: 0 is sold out, "ไม่จำกัด"
/// has no set number, and what the step can't take is refused with why.
#[tokio::test]
async fn the_stock_is_what_is_left_for_new_orders() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่เบอร์ 2", Some(30)).await;

    s.changes(eggs, "stock").await;
    let sold_out = s.types("0").await;
    assert_eq!(sold_out[0], t.product_stock_saved(Some(0), "แผง", false));
    let now = card(&sold_out[1]);
    assert_eq!(texts(now)[2], "หมด");
    assert_eq!(labels_of(now)[0], "เติมของ");
    assert_eq!(s.get(eggs).await.stock, Some(0));

    s.changes(eggs, "stock").await;
    let unlimited = s.types("ไม่จำกัด").await;
    // Sold out, it is orderable again
    assert_eq!(unlimited[0], t.product_stock_saved(None, "แผง", true));
    assert_eq!(row(card(&unlimited[1]), "จำนวน"), "ไม่จำกัด / ทำตามสั่ง");
    assert_eq!(s.get(eggs).await.stock, None);

    s.changes(eggs, "stock").await;
    let kept = s.types("ใช้ของเดิม").await;
    assert_eq!(kept[0], t.product_unchanged());
    assert_eq!(s.flow(s.seller).await, None);

    s.changes(eggs, "stock").await;
    let too_many = s.types("100000").await;
    assert_eq!(text(&too_many[0]), text(&t.stock_too_large(MAX_STOCK)));
    let not_a_number = s.types("สามสิบ").await;
    assert_eq!(
        text(&not_a_number[0]),
        t.number_problem(NumberError::NotANumber)
    );
    assert_eq!(
        quick_replies(&not_a_number[0]),
        ["ไม่จำกัด / ทำตามสั่ง", "ใช้ของเดิม", "ยกเลิก"]
    );
    assert_eq!(s.flow(s.seller).await.as_deref(), Some(EditFlow::NAME));
    assert_eq!(s.get(eggs).await.stock, None);
    let edited = s.types("12 แผง").await;
    assert_eq!(edited[0], t.product_stock_saved(Some(12), "แผง", false));
    assert_eq!(text(&edited[0]), "แก้จำนวนเป็น 12 แผง แล้วค่ะ");
    assert_eq!(s.get(eggs).await.stock, Some(12));
}

/// FR-PROD-05, FR-SRCH-03: the shop's products but the deleted ones, ten to a page with
/// "ดูเพิ่มเติม", sold-out ones first, then the newest; each card with its photo, rows and
/// buttons.
#[tokio::test]
async fn the_list_pages_the_shops_products_sold_out_first() {
    let s = setup().await;
    let mut ids = Vec::new();
    for n in 1..=13 {
        let stock = if [3, 7].contains(&n) {
            Some(0)
        } else {
            Some(5)
        };
        ids.push(s.product(&format!("สินค้า {n}"), stock).await);
    }
    s.set_status(ids[4], product::Status::Hidden).await;
    s.set_status(ids[5], product::Status::Deleted).await;
    s.list_in(s.other_shop, "ไข่ของร้านอื่น", Some(5)).await;
    let front = s.add_photo(ids[12]).await;
    s.add_photo(ids[12]).await;

    let first = s.list().await;
    let Body::Flex { alt_text, .. } = &first.body else {
        panic!("not a card");
    };
    assert_eq!(alt_text, "สินค้าของฉัน");
    assert_eq!(quick_replies(&first), ["เพิ่มสินค้า", "เมนู"]);
    assert_eq!(quick_reply_data(&first), [Some("add_product"), None]);
    let page = cards(&first);
    assert_eq!(page.len(), PRODUCTS_PER_PAGE + 1);
    let names: Vec<_> = page[..PRODUCTS_PER_PAGE]
        .iter()
        .map(|card| texts(card)[0].clone())
        .collect();
    assert_eq!(
        names,
        [7, 3, 13, 12, 11, 10, 9, 8, 5, 4].map(|n| format!("สินค้า {n}"))
    );
    let more = &page[PRODUCTS_PER_PAGE].body.as_ref().unwrap().contents[0];
    let Component::Button(more) = more else {
        panic!("not a button: {more:?}");
    };
    let next = Action::postback(labels::MORE.clone(), "my_products?page=1");
    assert_eq!(more.action, next);

    // The newest, with its photos
    let newest = page[2];
    assert_eq!(hero(newest), Some(s.photo_url(front).as_str()));
    assert_eq!(row(newest, "รูปสินค้า"), "2 รูป");
    assert_eq!(row(newest, "หมวดหมู่"), "ไข่/นม");
    assert_eq!(row(newest, "จำนวน"), "5 แผง");
    let id = ids[12];
    assert_eq!(
        buttons(newest),
        [
            ("แก้จำนวน", format!("my_products.part?id={id}&p=stock")),
            ("แก้ราคา", format!("my_products.part?id={id}&p=price")),
            ("เปลี่ยนรูป", format!("my_products.part?id={id}&p=photos")),
            ("แก้ไข", format!("my_products.edit?id={id}")),
            ("ซ่อนสินค้า", format!("my_products.show?id={id}&to=0")),
            ("ลบ", format!("my_products.delete?id={id}")),
        ]
        .map(|(label, data)| (label.to_owned(), data))
    );
    // Without photos, and hidden
    assert_eq!(hero(page[3]), None);
    assert_eq!(row(page[3], "รูปสินค้า"), "ไม่มี");
    let hidden = page[8];
    assert_eq!(texts(hidden)[2], "ซ่อนอยู่ (ผู้ซื้อไม่เห็น)");
    assert_eq!(labels_of(hidden)[4], "แสดงสินค้า");

    let second = s.taps(s.seller, "my_products?page=1").await;
    let names: Vec<_> = cards(&second[0])
        .iter()
        .map(|card| texts(card)[0].clone())
        .collect();
    assert_eq!(names, ["สินค้า 2", "สินค้า 1"]);
    // A page past the last, from an old button, starts over
    let past = s.taps(s.seller, "my_products?page=9").await;
    assert_eq!(past, [first]);
}

/// FR-UI-03: without a product, "สินค้าของฉัน" offers to list one.
#[tokio::test]
async fn without_products_the_list_offers_to_add_one() {
    let s = setup().await;
    let deleted = s.product("ไข่ไก่เบอร์ 2", Some(5)).await;
    s.set_status(deleted, product::Status::Deleted).await;
    let add = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    let empty = s.templates.my_products(&[], 0, |_| unreachable!(), add);
    assert_eq!(s.list().await, empty);
    assert_eq!(
        text(&empty),
        "ร้านยังไม่มีสินค้าเลยค่ะ กด \"เพิ่มสินค้า\" เพื่อลงขายได้เลยค่ะ"
    );
}

/// FR-PROD-05, FR-PROD-03: "แก้ไข" offers each part; each is asked for as "เพิ่มสินค้า" asks,
/// showing what it is now, and saved alone once answered.
#[tokio::test]
async fn the_details_are_changed_a_part_at_a_time() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;

    let choose = s
        .taps(s.seller, &format!("my_products.edit?id={eggs}"))
        .await;
    assert_eq!(choose.len(), 2);
    assert_eq!(choose[0], t.editing_product("ไข่ไก่"));
    assert_eq!(
        quick_replies(&choose[1]),
        [
            "รูปสินค้า",
            "ชื่อสินค้า",
            "ราคา",
            "หน่วย",
            "หมวดหมู่",
            "จำนวน",
            "รายละเอียด"
        ]
    );
    assert_eq!(
        quick_reply_data(&choose[1])[1],
        Some(format!("my_products.part?id={eggs}&p=name").as_str())
    );
    // Nothing starts until a part is chosen
    assert_eq!(s.flow(s.seller).await, None);

    let name = s.changes(eggs, "name").await;
    assert!(
        text(&name[1]).ends_with("(ตอนนี้: ไข่ไก่)"),
        "{}",
        text(&name[1])
    );
    let named = s.types("  ไข่ไก่   เบอร์ 0 ").await;
    assert_eq!(named[0], t.product_saved());
    assert_eq!(texts(card(&named[1]))[0], "ไข่ไก่ เบอร์ 0");

    s.changes(eggs, "unit").await;
    s.types("กล่อง").await;

    let category = s.changes(eggs, "category").await;
    assert_eq!(
        quick_replies(&category[1]),
        ["ไข่/นม", "ขนม/ของหวาน", "ใช้ของเดิม", "ยกเลิก"]
    );
    let inactive = s.types("ผลไม้").await;
    assert_eq!(
        inactive[0],
        with_choices_of(&category[1], t.category_inactive())
    );
    let moved = s.types("ขนม/ของหวาน").await;
    assert_eq!(row(card(&moved[1]), "หมวดหมู่"), "ขนม/ของหวาน");

    let description = s.changes(eggs, "description").await;
    assert_eq!(quick_replies(&description[1]), ["ข้าม", "ยกเลิก"]);
    s.types("ไข่สดจากฟาร์มทุกเช้า").await;
    let description = s.changes(eggs, "description").await;
    assert_eq!(
        quick_replies(&description[1]),
        ["ลบออก", "ใช้ของเดิม", "ยกเลิก"]
    );
    assert_eq!(
        s.get(eggs).await.description.as_deref(),
        Some("ไข่สดจากฟาร์มทุกเช้า")
    );
    s.types("ลบออก").await;

    let product = s.get(eggs).await;
    assert_eq!(
        (
            product.name.as_str(),
            product.unit.as_str(),
            product.category_id,
            product.description,
        ),
        ("ไข่ไก่ เบอร์ 0", "กล่อง", s.sweets, None)
    );
    // The rest as it was
    assert_eq!(
        (product.price, product.stock, product.status),
        (Money::from_baht(110), Some(30), product::Status::Active)
    );
    assert_eq!(s.flow(s.seller).await, None);
}

/// `message` with the Quick Replies of `asked`, the question that refused an answer.
fn with_choices_of(asked: &Message, message: Message) -> Message {
    let mut message = message;
    message.quick_reply = asked.quick_reply.clone();
    message
}

/// FR-LINE-11: while the category is being changed, "ดูเพิ่มเติม" pages the categories within
/// "สินค้าของฉัน"; once the change is over, it shows the list.
#[tokio::test]
async fn categories_are_paged_while_changing_one() {
    let s = setup().await;
    for n in 4..=13 {
        let category = category::ActiveModel {
            sort_order: Set(n),
            ..testing::new_category(&format!("หมวด {n}"))
        };
        category.insert(&s.db).await.unwrap();
    }
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    let ask = s.changes(eggs, "category").await;
    let choices = quick_replies(&ask[1]);
    assert_eq!(choices.len(), limits::QUICK_REPLY_ITEMS);
    assert_eq!(choices[10..], ["ดูเพิ่มเติม", "ใช้ของเดิม", "ยกเลิก"]);
    assert_eq!(
        quick_reply_data(&ask[1])[10],
        Some("my_products.categories?page=1")
    );
    let more = s.taps(s.seller, "my_products.categories?page=1").await;
    assert_eq!(
        quick_replies(&more[0]),
        ["หมวด 12", "หมวด 13", "ใช้ของเดิม", "ยกเลิก"]
    );
    s.types("หมวด 13").await;
    let product = s.get(eggs).await;
    let category = category::Entity::find_by_id(product.category_id)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(category.name, "หมวด 13");

    let over = s.taps(s.seller, "my_products.categories?page=1").await;
    assert_eq!(over, [s.list().await]);
}

/// FR-PROD-04, FR-PROD-05: photos are added to the product's up to the most it may have, saved
/// once there are that many or "ถัดไป" is tapped, or replaced, "ลบออก" first; the replaced ones
/// are deleted.
#[tokio::test]
async fn photos_are_added_or_replaced() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    let first = s.add_photo(eggs).await;

    let ask = s.changes(eggs, "photos").await;
    assert_eq!(text(&ask[1]), text(&t.ask_product_photos(1, 3, true)));
    assert_eq!(
        quick_replies(&ask[1]),
        ["ถ่ายรูป", "เลือกรูป", "ถัดไป", "ลบออก", "ยกเลิก"]
    );
    let second = s.sends_photo("m1", PHOTO).await.expect("taken");
    assert_eq!(text(&second[0]), text(&t.ask_product_photos(2, 3, true)));
    let drafts: Vec<_> = s
        .images()
        .await
        .into_iter()
        .filter(|&(owner_type, ..)| owner_type == OwnerType::Draft)
        .collect();
    let [(_, owner, _, back)] = drafts[..] else {
        panic!("one draft: {drafts:?}");
    };
    assert_eq!(owner, s.seller);
    let saved = s.types("ถัดไป").await;
    assert_eq!(saved[0], t.product_saved());
    assert_eq!(row(card(&saved[1]), "รูปสินค้า"), "2 รูป");
    assert_eq!(hero(card(&saved[1])), Some(s.photo_url(first).as_str()));
    assert_eq!(
        s.images().await,
        [
            (OwnerType::Product, eggs, 0, first),
            (OwnerType::Product, eggs, 1, back)
        ]
    );

    // The third is saved as it comes, the most a product has
    s.changes(eggs, "photos").await;
    let full = s.sends_photo("m2", PHOTO).await.expect("taken");
    assert_eq!(full[0], t.product_saved());
    assert_eq!(s.flow(s.seller).await, None);
    let images = s.images().await;
    assert_eq!(images.len(), 3);
    assert!(
        images
            .iter()
            .zip(0..)
            .all(
                |(&(owner_type, owner, position, _), at)| (owner_type, owner, position)
                    == (OwnerType::Product, eggs, at)
            )
    );

    // A fourth isn't even downloaded
    let ask = s.changes(eggs, "photos").await;
    assert_eq!(quick_replies(&ask[1]), ["ถัดไป", "ลบออก", "ยกเลิก"]);
    let refused = s.sends_photo("m3", PHOTO).await.expect("an answer");
    assert_eq!(refused[0], t.product_photos_full(3));

    // Replaced: nothing changes until the new ones are saved
    let removed = s.types("ลบออก").await;
    assert_eq!(text(&removed[0]), text(&t.ask_product_photos(0, 3, true)));
    assert_eq!(
        quick_replies(&removed[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    assert_eq!(s.images().await, images);
    s.sends_photo("m4", PHOTO).await.expect("taken");
    s.types("ถัดไป").await;
    let [(OwnerType::Product, owner, 0, new)] = s.images().await[..] else {
        panic!("one photo: {:?}", s.images().await);
    };
    assert_eq!(owner, eggs);
    assert!(images.iter().all(|&(.., id)| id != new));

    // Or none at all
    s.changes(eggs, "photos").await;
    s.types("ลบออก").await;
    let none = s.types("ข้าม").await;
    assert_eq!(row(card(&none[1]), "รูปสินค้า"), "ไม่มี");
    assert_eq!(s.images().await, []);

    // FR-UI-14: a photo where the price is asked for isn't taken, and goes on to be offered
    // for listing
    s.changes(eggs, "price").await;
    assert_eq!(s.sends_photo("m5", PHOTO).await, None);
    assert_eq!(s.flow(s.seller).await.as_deref(), Some(EditFlow::NAME));
}

/// FR-PROD-05, FR-PROD-07, FR-UI-13: hiding and showing is a tap whose button names the state
/// it sets, so tapping it again changes nothing; a product held for review can be neither, and
/// a removed one can only be deleted.
#[tokio::test]
async fn hiding_and_showing_is_a_tap() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    let hide = format!("my_products.show?id={eggs}&to=0");

    let hidden = s.taps(s.seller, &hide).await;
    assert_eq!(hidden.len(), 2);
    assert_eq!(hidden[0], t.product_visibility("ไข่ไก่", false, true));
    let now = card(&hidden[1]);
    assert_eq!(texts(now)[2], "ซ่อนอยู่ (ผู้ซื้อไม่เห็น)");
    let show = format!("my_products.show?id={eggs}&to=1");
    assert_eq!(buttons(now)[4], ("แสดงสินค้า".to_owned(), show.clone()));
    assert_eq!(s.get(eggs).await.status, product::Status::Hidden);

    let again = s.taps(s.seller, &hide).await;
    assert_eq!(again[0], t.product_visibility("ไข่ไก่", false, false));
    assert_eq!(s.get(eggs).await.status, product::Status::Hidden);

    let shown = s.taps(s.seller, &show).await;
    assert_eq!(shown[0], t.product_visibility("ไข่ไก่", true, true));
    assert_eq!(s.get(eggs).await.status, product::Status::Active);

    // Held for review (#27), it can still be changed and deleted
    s.set_status(eggs, product::Status::Held).await;
    let held = s.taps(s.seller, &hide).await;
    assert_eq!(held.len(), 1);
    let now = card(&held[0]);
    assert_eq!(texts(now)[2], "รอตรวจ (ผู้ซื้อยังไม่เห็น)");
    assert_eq!(
        labels_of(now),
        ["แก้จำนวน", "แก้ราคา", "เปลี่ยนรูป", "แก้ไข", "ลบ"]
    );
    assert_eq!(s.get(eggs).await.status, product::Status::Held);

    // Removed by an admin (#31), it can only be deleted
    s.set_status(eggs, product::Status::Removed).await;
    let removed = s.changes(eggs, "price").await;
    assert_eq!(removed.len(), 1);
    assert_eq!(labels_of(card(&removed[0])), ["ลบ"]);
    assert_eq!(s.flow(s.seller).await, None);
}

/// FR-PROD-05, DR-04, FR-UI-13: deleting is confirmed first, and only once; the product stays,
/// `DELETED`, with its photos until retention, and leaves the list; an old button finds it gone.
#[tokio::test]
async fn a_deleted_product_is_kept_and_leaves_the_list() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    let photo = s.add_photo(eggs).await;

    let ask = s
        .taps(s.seller, &format!("my_products.delete?id={eggs}"))
        .await;
    let confirm = quick_reply_data(&ask[0])[0].unwrap().to_owned();
    let id = Postback::parse(&confirm).unwrap().uuid("pa").unwrap();
    let button = Postback::new(pending::CONFIRM)
        .with_name("k", DeleteProduct::KIND)
        .with_uuid("pa", id)
        .button(labels::CONFIRM_DELETE.clone());
    assert_eq!(ask, [t.confirm_delete_product("ไข่ไก่", button)]);
    assert_eq!(quick_replies(&ask[0]), ["ยืนยันลบ", "ยกเลิก"]);
    assert_eq!(s.get(eggs).await.status, product::Status::Active);

    let deleted = s.taps(s.seller, &confirm).await;
    assert_eq!(deleted, [t.product_deleted("ไข่ไก่", true)]);
    assert_eq!(quick_replies(&deleted[0]), ["สินค้าของฉัน", "เมนู"]);
    let product = s.get(eggs).await;
    assert_eq!(
        (product.status, product.deleted_at, product.updated_at),
        (product::Status::Deleted, Some(t0()), t0())
    );
    assert_eq!(s.images().await, [(OwnerType::Product, eggs, 0, photo)]);
    assert!(matches!(s.list().await.body, Body::Text { .. }));

    // Again, nothing more happens
    assert_eq!(s.taps(s.seller, &confirm).await, deleted);
    assert_eq!(s.get(eggs).await, product);
    // An old card's buttons find it gone
    let gone = s.changes(eggs, "price").await;
    assert_eq!(gone, [t.denied(Denied::NotFound), s.list().await]);
    assert_eq!(s.flow(s.seller).await, None);

    // Not confirmed in time, it is kept
    let ham = s.product("แฮม", Some(5)).await;
    let ask = s
        .taps(s.seller, &format!("my_products.delete?id={ham}"))
        .await;
    let confirm = quick_reply_data(&ask[0])[0].unwrap().to_owned();
    let late = s
        .taps_at(s.seller, &confirm, t0() + TimeDelta::hours(2))
        .await;
    assert_eq!(late, [t.product_deleted("แฮม", false)]);
    assert_eq!(s.get(ham).await.status, product::Status::Active);
}

/// A part answered after the product was deleted finds it gone; nothing is written.
#[tokio::test]
async fn a_product_deleted_meanwhile_is_gone() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    s.changes(eggs, "price").await;
    s.set_status(eggs, product::Status::Deleted).await;
    let gone = s.types("120").await;
    assert_eq!(gone[0], t.denied(Denied::NotFound));
    assert_eq!(s.get(eggs).await.price, Money::from_baht(110));
    assert_eq!(s.flow(s.seller).await, None);
}

/// NFR-SEC-02, FR-SHOP-10, NFR-SEC-10: only the owner of an approved shop manages its products,
/// and only its own; a suspension ends a change in progress.
#[tokio::test]
async fn only_the_owner_of_an_approved_shop_manages_its_products() {
    let s = setup().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    let others = s.list_in(s.other_shop, "ไข่ของร้านอื่น", Some(5)).await;

    assert_eq!(
        s.taps(s.buyer, "my_products").await,
        [t.denied(Denied::NotAllowed)]
    );
    for data in [
        format!("my_products.part?id={eggs}&p=price"),
        format!("my_products.edit?id={eggs}"),
        format!("my_products.show?id={eggs}&to=0"),
        format!("my_products.delete?id={eggs}"),
    ] {
        let denied = [t.denied(Denied::NotAllowed)];
        assert_eq!(s.taps(s.other_seller, &data).await, denied, "{data}");
        assert_eq!(s.taps(s.buyer, &data).await, denied, "{data}");
    }
    assert_eq!(s.get(eggs).await.status, product::Status::Active);
    assert_eq!(s.flow(s.other_seller).await, None);
    let mine = format!("my_products.part?id={others}&p=price");
    assert_eq!(
        s.taps(s.seller, &mine).await,
        [t.denied(Denied::NotAllowed)]
    );
    assert_eq!(
        s.taps(s.seller, "my_products.part?id=999999&p=price").await,
        [t.denied(Denied::NotFound)]
    );

    // A suspended shop's owner can't, and a change in progress ends
    s.changes(eggs, "price").await;
    let mut shop = s.shop().await.into_active_model();
    shop.status = Set(shop::Status::Suspended);
    shop.update(&s.db).await.unwrap();
    assert_eq!(s.types("120").await, [t.denied(Denied::ShopSuspended)]);
    assert_eq!(s.flow(s.seller).await, None);
    assert_eq!(s.get(eggs).await.price, Money::from_baht(110));
    assert_eq!(
        s.taps(s.seller, "my_products").await,
        [t.denied(Denied::ShopSuspended)]
    );
}

/// FR-PROD-09: a changed name, unit or description with a prohibited keyword holds the product,
/// active or hidden, for review: the seller is told, a report of no one's is filed and the
/// admins are alerted. Its price and stock aren't moderated, and once held it changes as before
/// without being checked or filed again.
#[tokio::test]
async fn text_found_suspicious_holds_the_product() {
    let mut s = setup().await;
    s.config.catalog.prohibited_keywords = vec!["กัญชา".to_owned()];
    // On, but never asked: a keyword decides alone
    let (ai, model) = scripted::scripted(&s.pool, [], true, &[]);
    s.ai = ai;
    s.admin().await;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;

    s.changes(eggs, "price").await;
    s.types("115").await;
    assert_eq!(s.get(eggs).await.status, product::Status::Active);

    s.changes(eggs, "name").await;
    let held = s.types("ไข่ไก่ใส่กัญชา").await;
    assert_eq!(held[0], t.product_held_saved());
    assert_eq!(texts(card(&held[1]))[2], "รอตรวจ (ผู้ซื้อยังไม่เห็น)");
    let product = s.get(eggs).await;
    assert_eq!(
        (product.name.as_str(), product.status),
        ("ไข่ไก่ใส่กัญชา", product::Status::Held)
    );
    let reason = "มีคำต้องห้าม \"กัญชา\"".to_owned();
    let filed = (None, report::TargetType::Product, eggs, reason);
    assert_eq!(s.reports().await, std::slice::from_ref(&filed));
    assert_eq!(s.pushes().await, ["admin.held_for_review"]);

    // Held, it is changed as before, and not checked or filed again
    s.changes(eggs, "description").await;
    let saved = s.types("กัญชาแท้").await;
    assert_eq!(saved[0], t.product_saved());
    assert_eq!(s.get(eggs).await.description.as_deref(), Some("กัญชาแท้"));
    assert_eq!(s.reports().await, [filed]);
    assert_eq!(s.pushes().await.len(), 1);

    // Hidden, it is held too
    let sweets = s.product("ขนมครก", Some(5)).await;
    s.set_status(sweets, product::Status::Hidden).await;
    s.changes(sweets, "unit").await;
    let held = s.types("ห่อกัญชา").await;
    assert_eq!(held[0], t.product_held_saved());
    assert_eq!(s.get(sweets).await.status, product::Status::Held);
    assert_eq!(s.reports().await.len(), 2);
    assert!(model.requests().is_empty());
}

/// FR-PROD-09, FR-AI-12: without a keyword the AI decides, seeing the product's text as it will
/// be and only the photos new to it.
#[tokio::test]
async fn the_ai_moderates_changed_text_and_new_photos() {
    let mut s = setup().await;
    let turns = [
        scripted::Turn::Json(json!({ "verdict": "allow", "reason": "" })),
        scripted::Turn::Json(json!({ "verdict": "review", "reason": "รูปไม่ตรงกับสินค้า" })),
    ];
    let (ai, model) = scripted::scripted(&s.pool, turns, true, &[]);
    s.ai = ai;
    let t = &s.templates;
    let eggs = s.product("ไข่ไก่", Some(30)).await;
    s.add_photo(eggs).await;

    s.changes(eggs, "name").await;
    let saved = s.types("ไข่ไก่สด").await;
    assert_eq!(saved[0], t.product_saved());
    assert_eq!(s.get(eggs).await.status, product::Status::Active);
    let asked = model.requests()[0].messages[0].text();
    let listing = json!({ "name": "ไข่ไก่สด", "unit": "แผง" });
    assert!(asked.contains(&ai::as_data("listing", &listing)), "{asked}");

    s.changes(eggs, "photos").await;
    s.sends_photo("m1", PHOTO).await.expect("taken");
    let held = s.types("ถัดไป").await;
    assert_eq!(held[0], t.product_held_saved());
    assert_eq!(s.get(eggs).await.status, product::Status::Held);
    let reason = "AI: รูปไม่ตรงกับสินค้า".to_owned();
    assert_eq!(
        s.reports().await,
        [(None, report::TargetType::Product, eggs, reason)]
    );
    // The photo it had isn't sent again
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let photos = requests[1].messages[0]
        .parts
        .iter()
        .filter(|part| matches!(part, ai::Part::Image(_)))
        .count();
    assert_eq!(photos, 1);
}
