//! Adding a product (FR-PROD-01, FR-PROD-03): the flow's steps and what each refuses, the
//! photos kept as drafts until the product is listed, nothing listed before "ยืนยันลงขาย",
//! correcting a part, the categories paged, a seller's photo offered for listing, and only the
//! owner of an approved shop listing; the statuses a product goes through (FR-PROD-07); and
//! whether it can be ordered (FR-SRCH-07). Each event is handled in a transaction committed once
//! it is handled. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::sea_query::Expr;
use sea_orm::{DatabaseConnection, IntoActiveModel, Iterable, QueryOrder};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::{Finished, Location};
use crate::line::Body;
use crate::line::flex::{Component, FlexContainer, Hero};
use crate::line::testing::{self as fake, Fake};
use crate::notifications::templates::{Templates, with_cancel};
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{session, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// The JPEG LINE serves for the photos users send
const PHOTO: &[u8] = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

#[test]
fn the_steps_are_asked_in_order() {
    let mut draft = ProductFlow::new(Vec::new());
    let mut step = Step::Photos;
    let mut asked = vec![step];
    while step != Step::Summary {
        // What answering the step leaves, as far as the order goes
        match step {
            Step::Name => draft.name = Some("ไข่ไก่".to_owned()),
            Step::Price => draft.price = Some(Money::from_baht(110)),
            Step::Unit => draft.unit = Some("แผง".to_owned()),
            Step::Category => draft.category_id = Some(1),
            Step::Stock => draft.stock = Some(Stock::Unlimited),
            _ => {}
        }
        step = after(&draft, step);
        asked.push(step);
    }
    assert_eq!(
        asked,
        [
            Step::Photos,
            Step::Name,
            Step::Price,
            Step::Unit,
            Step::Category,
            Step::Stock,
            Step::Description,
            Step::Summary,
        ]
    );

    // Correcting, each part leads back to the preview, unless one the product needs is missing
    draft.editing = true;
    for part in Step::PARTS {
        assert_eq!(after(&draft, part), Step::Summary, "{part:?}");
    }
    draft.category_id = None;
    assert_eq!(after(&draft, Step::Name), Step::Category);
}

#[test]
fn every_part_has_its_own_postback_and_label() {
    for step in Step::PARTS {
        assert_eq!(Step::part(step.key()), Some(step));
        let data = Postback::new(PART)
            .with_name(WHICH, step.key())
            .with_int(REVISION, i64::MAX)
            .to_string();
        assert!(data.len() <= limits::ACTION_TEXT_CHARS, "{data}");
    }
    assert_eq!(Step::part("summary"), None);
    let labels: Vec<_> = Step::PARTS.iter().map(|part| part.label()).collect();
    for (i, label) in labels.iter().enumerate() {
        assert!(!labels[..i].contains(label), "{label:?}");
    }
    // The suggested units and the cancel button are as many Quick Replies as LINE takes
    assert_eq!(labels::UNITS.len() + 1, limits::QUICK_REPLY_ITEMS);
}

/// FR-PROD-03, FR-UI-06: a price is ฿0.01 – ฿100,000 with at most 2 decimals, in Arabic or Thai
/// digits.
#[test]
fn a_price_is_from_a_satang_to_a_hundred_thousand_baht() {
    for (text, satang) in [
        ("0.01", 1),
        ("20", 2_000),
        ("๒๐ บาท", 2_000),
        ("12.5", 1_250),
        ("฿1,250.50", 125_050),
        ("100,000", 10_000_000),
    ] {
        assert_eq!(price(text), Ok(Money::from_satang(satang)), "{text:?}");
    }
    assert_eq!(price("0"), Err(Refused::TooCheap));
    assert_eq!(price("0.00"), Err(Refused::TooCheap));
    assert_eq!(price("100,000.01"), Err(Refused::TooMuch(MAX_PRICE)));
    assert_eq!(price(&"9".repeat(25)), Err(Refused::TooMuch(MAX_PRICE)));
    assert_eq!(
        price("12.555"),
        Err(Refused::NotANumber(NumberError::TooManyDecimals))
    );
    for text in ["ยี่สิบ", "-5", "20/แผง"] {
        assert_eq!(
            price(text),
            Err(Refused::NotANumber(NumberError::NotANumber)),
            "{text:?}"
        );
    }
    assert_eq!(price(""), Err(Refused::Empty));
}

/// FR-PROD-03: a stock is a whole number up to 99,999, which may be followed by the unit, or
/// "ไม่จำกัด / ทำตามสั่ง".
#[test]
fn a_stock_is_a_count_up_to_99999_or_unlimited() {
    let unit = Some("กล่อง");
    for (text, count) in [
        ("0", 0),
        ("30", 30),
        ("๓๐", 30),
        ("30 กล่อง", 30),
        ("30กล่อง", 30),
        ("99,999", 99_999),
    ] {
        assert_eq!(stock(text, unit), Ok(Stock::Count(count)), "{text:?}");
    }
    for text in ["ไม่จำกัด / ทำตามสั่ง", "ไม่จำกัด", "ทำตามสั่ง"]
    {
        assert_eq!(stock(text, unit), Ok(Stock::Unlimited), "{text:?}");
    }
    assert_eq!(stock("100000", unit), Err(Refused::TooMany(MAX_STOCK)));
    assert_eq!(
        stock(&"9".repeat(25), unit),
        Err(Refused::TooMany(MAX_STOCK))
    );
    assert_eq!(
        stock("2.5", unit),
        Err(Refused::NotANumber(NumberError::NotWhole))
    );
    for text in ["-1", "30 แผง", "สามสิบ"] {
        assert_eq!(
            stock(text, unit),
            Err(Refused::NotANumber(NumberError::NotANumber)),
            "{text:?}"
        );
    }
    assert_eq!(stock("กล่อง", unit), Err(Refused::Empty));
    assert_eq!(Stock::Count(0).column(), Some(0));
    assert_eq!(Stock::Unlimited.column(), None);
}

#[test]
fn names_and_units_are_on_one_line() {
    assert_eq!(one_line("  ไข่ไก่ \n  เบอร์ 2 "), "ไข่ไก่ เบอร์ 2");
    assert_eq!(
        bounded(&"ข".repeat(NAME_CHARS), NAME_CHARS).map(str::len),
        Ok(240)
    );
    assert_eq!(
        bounded(&"ข".repeat(NAME_CHARS + 1), NAME_CHARS),
        Err(Refused::TooLong(NAME_CHARS))
    );
    assert_eq!(bounded("", UNIT_CHARS), Err(Refused::Empty));
}

/// FR-PROD-07: `DRAFT` → `ACTIVE` ⇄ `HIDDEN`; held for review, then listed or removed; deleted
/// for good.
#[test]
fn statuses_change_as_fr_prod_07_says() {
    use product::Status::{Active, Deleted, Draft, Held, Hidden, Removed};
    let allowed = [
        (Draft, Active),
        (Draft, Held),
        (Active, Hidden),
        (Hidden, Active),
        (Active, Held),
        (Hidden, Held),
        (Held, Active),
        (Held, Removed),
        (Draft, Deleted),
        (Active, Deleted),
        (Hidden, Deleted),
        (Held, Deleted),
        (Removed, Deleted),
    ];
    for from in product::Status::iter() {
        for to in product::Status::iter() {
            assert_eq!(
                may_become(from, to),
                allowed.contains(&(from, to)),
                "{from:?} → {to:?}"
            );
        }
    }
}

fn shop_of(status: shop::Status, is_open: bool) -> shop::Model {
    shop::Model {
        id: 1,
        owner_id: 1,
        name: "ร้านป้าแดง".to_owned(),
        description: "ไข่ไก่".to_owned(),
        status,
        is_open,
        opening_hours: None,
        offers_pickup: true,
        offers_delivery: false,
        delivery_fee: Money::ZERO,
        minimum_order: None,
        promptpay_id: None,
        phone: None,
        show_phone: true,
        pickup_address: None,
        pickup_latitude: None,
        pickup_longitude: None,
        rejection_reason: None,
        created_at: t0(),
        updated_at: t0(),
        deleted_at: None,
    }
}

fn product_of(stock: Option<i32>) -> product::Model {
    product::Model {
        id: 1,
        shop_id: 1,
        category_id: 1,
        name: "ไข่ไก่".to_owned(),
        description: None,
        price: Money::from_baht(110),
        unit: "แผง".to_owned(),
        stock,
        status: product::Status::Active,
        created_at: t0(),
        updated_at: t0(),
        deleted_at: None,
    }
}

/// FR-SRCH-07, FR-PROD-06: an open shop's product is orderable while it has stock.
#[test]
fn an_open_shops_product_goes_by_its_stock() {
    let open = shop_of(shop::Status::Approved, true);
    let cases = [
        (Some(5), Availability::Left(5), true),
        (None, Availability::Orderable, true),
        (Some(0), Availability::SoldOut, false),
    ];
    for (stock, availability, orderable) in cases {
        assert_eq!(Availability::of(&product_of(stock), &open), availability);
        assert_eq!(availability.orderable(), orderable, "{availability:?}");
    }
}

/// FR-SHOP-05, FR-SHOP-10: a closed shop's products can't be ordered, sold out or not, and
/// nor can those of a shop that isn't approved.
#[test]
fn a_closed_shops_products_cannot_be_ordered() {
    for shop in [
        shop_of(shop::Status::Approved, false),
        shop_of(shop::Status::Suspended, true),
        shop_of(shop::Status::Pending, true),
        shop_of(shop::Status::Closed, true),
    ] {
        for stock in [Some(5), None, Some(0)] {
            let availability = Availability::of(&product_of(stock), &shop);
            assert_eq!(availability, Availability::ShopClosed, "{shop:?}");
            assert!(!availability.orderable());
        }
    }
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    templates: Templates,
    /// The owner of [`Setup::shop`]
    seller: i64,
    /// Approved and open
    shop: i64,
    /// A user without a shop
    buyer: i64,
    /// The active categories, in order: ไข่/นม and ขนม/ของหวาน. ผลไม้ follows, inactive.
    eggs: i64,
    sweets: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let shop = testing::new_shop(seller).insert(&db).await.unwrap().id;
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
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        seller,
        shop,
        buyer,
        eggs,
        sweets,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s with `handle`, as the worker does: in a transaction
    /// that commits once it is handled, with the user loaded afresh.
    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished { messages, .. } = turn.finish(messages);
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        messages
    }

    /// User `user_id` taps a button sending `data`.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            tapped(turn, &self.config, &postback).await
        })
        .await
    }

    /// User `user_id` answers the step they are on with `input`; `None` if the step doesn't
    /// take it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let line = self.line.client();
        let mut answered = None;
        self.handle(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<ProductFlow>());
            let session = session.expect("adding a product");
            answered = answer(turn, &self.config, &line, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(user_id, input).await.expect("an answer")
    }

    /// LINE serves `bytes` as the content of message `message_id`.
    fn serves(&self, message_id: &str, bytes: &[u8]) {
        let path = format!("/v2/bot/message/{message_id}/content");
        self.line
            .script(&path, [fake::Answer::bytes("image/jpeg", bytes)]);
    }

    /// User `user_id` sends a photo, which LINE serves as `bytes`.
    async fn sends_photo(&self, user_id: i64, message_id: &str, bytes: &[u8]) -> Vec<Message> {
        self.serves(message_id, bytes);
        let input = Input::Image {
            message_id: message_id.to_owned(),
        };
        self.sends(user_id, input).await.expect("an answer")
    }

    /// User `user_id`'s product in progress, if any.
    async fn session(&self, user_id: i64) -> Option<Session<ProductFlow>> {
        let row = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()?;
        if row.flow != ProductFlow::NAME {
            return None;
        }
        Some(Session {
            step: serde_json::from_value(Json::String(row.step)).unwrap(),
            draft: serde_json::from_value(row.data).unwrap(),
        })
    }

    async fn step(&self, user_id: i64) -> Step {
        self.session(user_id).await.expect("adding a product").step
    }

    async fn flow(&self, user_id: i64) -> Option<String> {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .map(|row| row.flow)
    }

    async fn revision(&self, user_id: i64) -> i64 {
        self.session(user_id)
            .await
            .expect("adding a product")
            .draft
            .revision
    }

    /// User `user_id` taps the latest preview's button for `action`.
    async fn taps_latest(&self, user_id: i64, action: &str) -> Vec<Message> {
        let revision = self.revision(user_id).await;
        self.taps(user_id, &format!("{action}?r={revision}")).await
    }

    /// User `user_id` taps the latest preview's button correcting `part`.
    async fn corrects(&self, user_id: i64, part: Step) -> Vec<Message> {
        let revision = self.revision(user_id).await;
        let data = format!("{PART}?p={}&r={revision}", part.key());
        self.taps(user_id, &data).await
    }

    /// User `user_id` adds eggs, from "เพิ่มสินค้า" to the preview: a photo, ฿110 a แผง, 30 of
    /// them, with a description.
    async fn fill(&self, user_id: i64) -> Vec<Message> {
        self.taps(user_id, "add_product").await;
        self.sends_photo(user_id, "m1", PHOTO).await;
        for answer in ["ถัดไป", "ไข่ไก่เบอร์ 2", "110", "แผง", "ไข่/นม", "30"]
        {
            self.types(user_id, answer).await;
        }
        self.types(user_id, "ไข่สดจากฟาร์มทุกเช้า").await
    }

    async fn products(&self) -> Vec<product::Model> {
        product::Entity::find()
            .order_by_asc(product::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    /// Every image, as (owner type, owner, ID), in their order.
    async fn images(&self) -> Vec<(OwnerType, i64, Uuid)> {
        image::Entity::find()
            .order_by_asc(image::Column::Position)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.owner_type, row.owner_id, row.id))
            .collect()
    }

    /// Sets the shop's `status`, as an admin's decision would.
    async fn set_status(&self, status: shop::Status) {
        let mut shop = shop::Entity::find_by_id(self.shop)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        shop.status = Set(status);
        shop.update(&self.db).await.unwrap();
    }

    /// Activates or deactivates category `id`, as an admin would.
    async fn set_active(&self, id: i64, active: bool) {
        category::Entity::update_many()
            .col_expr(category::Column::IsActive, Expr::value(active))
            .filter(category::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }

    async fn main_menu(&self, user_id: i64) -> Message {
        let actor = Actor::load(&self.db, user_id).await.unwrap().unwrap();
        menu::main_menu(&actor)
    }

    fn photo_url(&self, id: Uuid) -> String {
        images::product_image_url(&self.config.app.base_url, id).to_string()
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

/// The card of `message`.
fn bubble(message: &Message) -> &crate::line::flex::Bubble {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Bubble(bubble),
            ..
        } => bubble,
        body => panic!("not a card: {body:?}"),
    }
}

/// The postback data of the buttons of `message`'s card.
fn buttons(message: &Message) -> Vec<String> {
    let Some(footer) = &bubble(message).footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { data, .. } => data.clone(),
                action => panic!("not a postback: {action:?}"),
            },
            component => panic!("not a button: {component:?}"),
        })
        .collect()
}

/// The link of the photo across the top of `message`'s card.
fn hero(message: &Message) -> Option<&str> {
    match &bubble(message).hero {
        Some(Hero::Image(image)) => Some(&image.url),
        _ => None,
    }
}

/// The texts of `message`'s card, in order, the rows' labels and values among them.
fn card_texts(message: &Message) -> Vec<String> {
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
    for component in &bubble(message).body.as_ref().unwrap().contents {
        texts(component, &mut out);
    }
    out
}

/// The value of the row labelled `label` in `message`'s card.
fn row(message: &Message, label: &str) -> String {
    let texts = card_texts(message);
    let at = texts.iter().position(|text| text == label).expect(label);
    texts[at + 1].clone()
}

/// The buttons that follow a listed product (Appendix A.3).
fn listed_next() -> Vec<Action> {
    vec![
        Item::MyProducts
            .postback()
            .button(labels::MY_PRODUCTS.clone()),
        Item::AddProduct
            .postback()
            .button(labels::ADD_ANOTHER.clone()),
    ]
}

/// FR-PROD-01, acceptance: each step in turn, the preview, and the product listed only once
/// "ยืนยันลงขาย" is tapped, with its photos in the order sent. Until then buyers can't see it,
/// as there is no product; a second tap lists nothing more.
#[tokio::test]
async fn a_product_is_listed_only_once_confirmed() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    let first = s.taps(a, "add_product").await;
    assert_eq!(first.len(), 1);
    assert!(
        text(&first[0]).starts_with("มาลงสินค้ากันค่ะ ระบบจะถามทีละข้อ"),
        "{}",
        text(&first[0])
    );
    assert_eq!(
        quick_replies(&first[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    let one = s.sends_photo(a, "m1", PHOTO).await;
    assert_eq!(text(&one[0]), text(&t.ask_product_photos(1, 3, false)));
    assert_eq!(quick_replies(&one[0]), ["ถ่ายรูป", "เลือกรูป", "ถัดไป", "ยกเลิก"]);
    s.sends_photo(a, "m2", PHOTO).await;
    let [(OwnerType::Draft, _, front), (OwnerType::Draft, _, back)] = s.images().await[..] else {
        panic!("two drafts: {:?}", s.images().await);
    };

    let name = s.types(a, "ถัดไป").await;
    assert_eq!(name, [with_cancel(t.ask_product_name(NAME_CHARS, None))]);
    let price = s.types(a, "  ไข่ไก่   เบอร์ 2 ").await;
    assert_eq!(price, [with_cancel(t.ask_product_price(MAX_PRICE, None))]);
    let unit = s.types(a, "๑๑๐ บาท").await;
    assert_eq!(
        quick_replies(&unit[0]),
        [
            "ชิ้น",
            "กล่อง",
            "ถุง",
            "แพ็ค",
            "กิโลกรัม",
            "ขีด",
            "แผง",
            "หวี",
            "กำ",
            "จาน",
            "ชุด",
            "ครั้ง",
            "ยกเลิก"
        ]
    );
    let category = s.types(a, "แผง").await;
    assert_eq!(text(&category[0]), text(&t.ask_product_category(None)));
    assert_eq!(
        quick_replies(&category[0]),
        ["ไข่/นม", "ขนม/ของหวาน", "ยกเลิก"]
    );
    let stock = s.types(a, "ไข่/นม").await;
    assert!(text(&stock[0]).starts_with("ตอนนี้มีขายกี่แผงคะ"));
    assert_eq!(quick_replies(&stock[0]), ["ไม่จำกัด / ทำตามสั่ง", "ยกเลิก"]);
    let description = s.types(a, "30 แผง").await;
    assert_eq!(quick_replies(&description[0]), ["ข้าม", "ยกเลิก"]);

    let preview = s.types(a, "ไข่สดจากฟาร์มทุกเช้า").await;
    assert_eq!(preview.len(), 2);
    assert!(text(&preview[0]).contains("กด \"ยืนยันลงขาย\" แล้วลูกค้าจะเห็นสินค้าทันที"));
    assert_eq!(hero(&preview[1]), Some(s.photo_url(front).as_str()));
    assert_eq!(
        card_texts(&preview[1])[..3],
        ["ไข่ไก่ เบอร์ 2", "฿110 / แผง", "ไข่สดจากฟาร์มทุกเช้า"]
    );
    assert_eq!(row(&preview[1], "หมวดหมู่"), "ไข่/นม");
    assert_eq!(row(&preview[1], "จำนวน"), "30 แผง");
    assert_eq!(row(&preview[1], "รูปสินค้า"), "2 รูป");
    let revision = s.revision(a).await;
    assert_eq!(
        buttons(&preview[1]),
        [
            format!("add_product.submit?r={revision}"),
            format!("add_product.edit?r={revision}")
        ]
    );
    // Nothing is listed yet, and the photos are still the seller's drafts
    assert_eq!(s.products().await, []);
    assert_eq!(
        s.images().await,
        [(OwnerType::Draft, a, front), (OwnerType::Draft, a, back)]
    );

    let listed = s.taps_latest(a, SUBMIT).await;
    assert_eq!(listed, [t.product_listed("ไข่ไก่ เบอร์ 2", listed_next())]);
    assert_eq!(
        text(&listed[0]),
        "ลงขาย ไข่ไก่ เบอร์ 2 แล้วค่ะ ลูกค้าเห็นสินค้านี้แล้วค่ะ"
    );
    let [product] = &s.products().await[..] else {
        panic!("one product: {:?}", s.products().await);
    };
    assert_eq!(
        (
            product.shop_id,
            product.category_id,
            product.name.as_str(),
            product.description.as_deref(),
        ),
        (s.shop, s.eggs, "ไข่ไก่ เบอร์ 2", Some("ไข่สดจากฟาร์มทุกเช้า"))
    );
    assert_eq!(
        (
            product.price,
            product.unit.as_str(),
            product.stock,
            product.status,
            product.created_at,
            product.deleted_at,
        ),
        (
            Money::from_baht(110),
            "แผง",
            Some(30),
            product::Status::Active,
            t0(),
            None
        )
    );
    assert_eq!(
        s.images().await,
        [
            (OwnerType::Product, product.id, front),
            (OwnerType::Product, product.id, back)
        ]
    );
    assert_eq!(s.flow(a).await, None);

    // FR-UI-13: a second tap of the same button lists nothing more
    let again = s.taps(a, &format!("{SUBMIT}?r={revision}")).await;
    let add = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    assert_eq!(again, [t.product_flow_gone(add)]);
    assert_eq!(s.products().await.len(), 1);
    assert_eq!(s.flow(a).await, None);
}

/// FR-UI-04: cancelling at the preview discards the product; nothing is listed.
#[tokio::test]
async fn cancelling_lists_nothing() {
    let s = setup().await;
    let a = s.seller;
    s.fill(a).await;
    let cancelled = s.handle(a, async |turn| flow::cancel(turn).await).await;
    assert_eq!(cancelled[0], s.templates.cancelled());
    assert_eq!(s.products().await, []);
    assert_eq!(s.flow(a).await, None);
    // Its photo is a draft, which the sweep deletes in time
    let [(OwnerType::Draft, owner, _)] = s.images().await[..] else {
        panic!("one draft: {:?}", s.images().await);
    };
    assert_eq!(owner, a);
}

/// Acceptance (FR-PROD-03): each step refuses what is out of range with a message saying what to
/// enter, and asks again; the longest and smallest values are taken.
#[tokio::test]
async fn each_step_refuses_what_it_cannot_take() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    s.taps(a, "add_product").await;

    let refused = s.types(a, "ไม่มีรูป").await;
    assert_eq!(text(&refused[0]), text(&t.photo_expected(&labels::SKIP)));
    assert_eq!(
        quick_replies(&refused[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    let not_a_photo = s.sends_photo(a, "m1", b"not a photo").await;
    assert_eq!(
        text(&not_a_photo[0]),
        t.image_rejected(Rejected::NotJpegOrPng)
    );
    let elsewhere = Input::Location(Location {
        latitude: 16.44,
        longitude: 102.83,
        address: None,
    });
    assert_eq!(s.sends(a, elsewhere).await, None);
    assert_eq!(s.step(a).await, Step::Photos);
    assert_eq!(s.images().await, []);
    s.types(a, "ข้าม").await;

    assert_eq!(
        s.types(a, &"ก".repeat(NAME_CHARS + 1)).await,
        [with_cancel(t.text_too_long(NAME_CHARS))]
    );
    assert_eq!(
        s.types(a, "   ").await,
        [with_cancel(t.ask_product_name(NAME_CHARS, None))]
    );
    assert_eq!(s.step(a).await, Step::Name);
    let longest = "ข".repeat(NAME_CHARS);
    s.types(a, &longest).await;

    for (typed, why) in [
        ("0", t.price_too_low(MIN_PRICE)),
        ("100,000.01", t.amount_too_large(MAX_PRICE)),
        (
            "12.555",
            Message::text(t.number_problem(NumberError::TooManyDecimals)),
        ),
        (
            "ยี่สิบบาท",
            Message::text(t.number_problem(NumberError::NotANumber)),
        ),
    ] {
        assert_eq!(s.types(a, typed).await, [with_cancel(why)], "{typed:?}");
    }
    assert_eq!(s.step(a).await, Step::Price);
    s.types(a, "0.01").await;

    let refused = s.types(a, &"ก".repeat(UNIT_CHARS + 1)).await;
    assert_eq!(text(&refused[0]), text(&t.text_too_long(UNIT_CHARS)));
    assert_eq!(quick_replies(&refused[0]).len(), limits::QUICK_REPLY_ITEMS);
    let longest_unit = "ข".repeat(UNIT_CHARS);
    s.types(a, &longest_unit).await;

    let refused = s.types(a, "ผลไม้").await;
    assert_eq!(text(&refused[0]), text(&t.category_inactive()));
    assert_eq!(
        quick_replies(&refused[0]),
        ["ไข่/นม", "ขนม/ของหวาน", "ยกเลิก"]
    );
    let again = s.types(a, "ของกิน").await;
    assert_eq!(text(&again[0]), text(&t.ask_product_category(None)));
    assert_eq!(s.step(a).await, Step::Category);
    s.types(a, "ขนม/ของหวาน").await;

    for (typed, why) in [
        ("100000", t.stock_too_large(MAX_STOCK)),
        (
            "2.5",
            Message::text(t.number_problem(NumberError::NotWhole)),
        ),
        (
            "-1",
            Message::text(t.number_problem(NumberError::NotANumber)),
        ),
    ] {
        let refused = s.types(a, typed).await;
        assert_eq!(text(&refused[0]), text(&why), "{typed:?}");
        assert_eq!(quick_replies(&refused[0]), ["ไม่จำกัด / ทำตามสั่ง", "ยกเลิก"]);
    }
    assert_eq!(s.step(a).await, Step::Stock);
    s.types(a, "0").await;

    let refused = s.types(a, &"ก".repeat(DESCRIPTION_CHARS + 1)).await;
    assert_eq!(text(&refused[0]), text(&t.text_too_long(DESCRIPTION_CHARS)));
    assert_eq!(quick_replies(&refused[0]), ["ข้าม", "ยกเลิก"]);
    let longest_description = "ข".repeat(DESCRIPTION_CHARS);
    let preview = s.types(a, &longest_description).await;
    assert_eq!(preview.len(), 2);
    assert_eq!(
        row(&preview[1], "จำนวน"),
        format!("0 {longest_unit} (ลูกค้าจะเห็นว่าหมด)")
    );
    assert_eq!(row(&preview[1], "รูปสินค้า"), "ไม่มี");
    assert_eq!(hero(&preview[1]), None);

    let Session { step, draft } = s.session(a).await.unwrap();
    assert_eq!(step, Step::Summary);
    assert_eq!(
        (
            draft.name.as_deref(),
            draft.price,
            draft.unit.as_deref(),
            draft.category_id,
            draft.stock,
            draft.description.as_deref(),
        ),
        (
            Some(longest.as_str()),
            Some(MIN_PRICE),
            Some(longest_unit.as_str()),
            Some(s.sweets),
            Some(Stock::Count(0)),
            Some(longest_description.as_str()),
        )
    );
    // Text at the preview shows it again
    assert_eq!(s.types(a, "ลงขายเลย").await.len(), 2);
    assert_eq!(s.products().await, []);

    s.taps_latest(a, SUBMIT).await;
    let [product] = &s.products().await[..] else {
        panic!("one product");
    };
    assert_eq!(
        (product.price, product.stock, product.category_id),
        (MIN_PRICE, Some(0), s.sweets)
    );
}

/// FR-PROD-04: photos are taken at any step, up to `image.maxPerProduct`; the photos step moves
/// on once they are all there, and one more isn't even downloaded. Correcting them, "ลบออก"
/// removes them for others to be sent.
#[tokio::test]
async fn photos_are_taken_at_any_step_up_to_the_most_a_product_has() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    s.taps(a, "add_product").await;
    s.sends_photo(a, "m1", PHOTO).await;
    s.types(a, "ถัดไป").await;
    let ask_name = with_cancel(t.ask_product_name(NAME_CHARS, None));
    assert_eq!(
        s.sends_photo(a, "m2", PHOTO).await,
        [t.product_photo_added(2, 3), ask_name.clone()]
    );
    assert_eq!(
        s.sends_photo(a, "m3", PHOTO).await,
        [t.product_photo_added(3, 3), ask_name.clone()]
    );
    assert_eq!(s.step(a).await, Step::Name);
    let fourth = Input::Image {
        message_id: "m4".to_owned(),
    };
    assert_eq!(
        s.sends(a, fourth).await,
        Some(vec![t.product_photos_full(3), ask_name])
    );
    assert!(
        !s.line
            .requests()
            .iter()
            .any(|request| request.path.contains("/m4/"))
    );
    assert_eq!(s.images().await.len(), 3);

    for answer in ["ไข่ไก่", "110", "แผง", "ไข่/นม", "30"] {
        s.types(a, answer).await;
    }
    let preview = s.types(a, "ข้าม").await;
    assert_eq!(row(&preview[1], "รูปสินค้า"), "3 รูป");

    // All there: no more can be sent, only kept or removed
    let photos = s.corrects(a, Step::Photos).await;
    assert_eq!(text(&photos[0]), text(&t.ask_product_photos(3, 3, true)));
    assert_eq!(quick_replies(&photos[0]), ["ถัดไป", "ลบออก", "ยกเลิก"]);
    let removed = s.types(a, "ลบออก").await;
    assert_eq!(text(&removed[0]), text(&t.ask_product_photos(0, 3, true)));
    assert_eq!(
        quick_replies(&removed[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    assert_eq!(s.images().await, []);
    assert_eq!(s.step(a).await, Step::Photos);
    let one = s.sends_photo(a, "m5", PHOTO).await;
    assert_eq!(
        quick_replies(&one[0]),
        ["ถ่ายรูป", "เลือกรูป", "ถัดไป", "ลบออก", "ยกเลิก"]
    );
    let [(OwnerType::Draft, _, new)] = s.images().await[..] else {
        panic!("one draft: {:?}", s.images().await);
    };
    let preview = s.types(a, "ถัดไป").await;
    assert_eq!(hero(&preview[1]), Some(s.photo_url(new).as_str()));
    // A photo at the preview joins the others, and the preview is shown again
    let shown = s.sends_photo(a, "m6", PHOTO).await;
    assert_eq!(shown[0], t.product_photo_added(2, 3));
    assert_eq!(row(&shown[2], "รูปสินค้า"), "2 รูป");

    s.taps_latest(a, SUBMIT).await;
    let product = s.products().await[0].id;
    let images = s.images().await;
    assert_eq!(images.len(), 2);
    assert_eq!(images[0], (OwnerType::Product, product, new));
}

/// Correcting from the preview: each part shows what it is now, with "ใช้ของเดิม", and leads
/// back to the preview, which lists the product as corrected.
#[tokio::test]
async fn correcting_a_part_returns_to_the_preview() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    s.fill(a).await;
    let parts = s.taps_latest(a, EDIT).await;
    assert_eq!(
        quick_replies(&parts[0]),
        [
            "รูปสินค้า",
            "ชื่อสินค้า",
            "ราคา",
            "หน่วย",
            "หมวดหมู่",
            "จำนวน",
            "รายละเอียด",
            "ยกเลิก"
        ]
    );

    let price = s.corrects(a, Step::Price).await;
    assert_eq!(
        text(&price[0]),
        text(&t.ask_product_price(MAX_PRICE, Some("฿110")))
    );
    assert_eq!(quick_replies(&price[0]), ["ใช้ของเดิม", "ยกเลิก"]);
    let preview = s.types(a, "115").await;
    assert_eq!(card_texts(&preview[1])[1], "฿115 / แผง");

    // The units leave room for "ใช้ของเดิม", which keeps the unit
    let unit = s.corrects(a, Step::Unit).await;
    let replies = quick_replies(&unit[0]);
    assert_eq!(replies.len(), limits::QUICK_REPLY_ITEMS);
    assert_eq!(replies[11..], ["ใช้ของเดิม", "ยกเลิก"]);
    let kept = s.types(a, "ใช้ของเดิม").await;
    assert_eq!(card_texts(&kept[1]), card_texts(&preview[1]));

    let stock = s.corrects(a, Step::Stock).await;
    assert!(
        text(&stock[0]).ends_with("(ตอนนี้: 30 แผง)"),
        "{}",
        text(&stock[0])
    );
    let preview = s.types(a, "ทำตามสั่ง").await;
    assert_eq!(row(&preview[1], "จำนวน"), "ไม่จำกัด / ทำตามสั่ง");

    let description = s.corrects(a, Step::Description).await;
    assert_eq!(
        quick_replies(&description[0]),
        ["ลบออก", "ใช้ของเดิม", "ยกเลิก"]
    );
    let preview = s.types(a, "ลบออก").await;
    assert_eq!(
        card_texts(&preview[1])[..3],
        ["ไข่ไก่เบอร์ 2", "฿115 / แผง", "หมวดหมู่"]
    );

    let category = s.corrects(a, Step::Category).await;
    assert_eq!(
        text(&category[0]),
        text(&t.ask_product_category(Some("ไข่/นม")))
    );
    assert_eq!(
        quick_replies(&category[0]),
        ["ไข่/นม", "ขนม/ของหวาน", "ใช้ของเดิม", "ยกเลิก"]
    );
    s.types(a, "ขนม/ของหวาน").await;

    s.taps_latest(a, SUBMIT).await;
    let [product] = &s.products().await[..] else {
        panic!("one product");
    };
    assert_eq!(
        (
            product.price,
            product.unit.as_str(),
            product.stock,
            product.category_id,
            product.description.as_deref()
        ),
        (Money::from_baht(115), "แผง", None, s.sweets, None)
    );
}

/// FR-LINE-11: more active categories than fit are paged with "ดูเพิ่มเติม", which acts only
/// while the category is asked for.
#[tokio::test]
async fn categories_are_paged() {
    let s = setup().await;
    let a = s.seller;
    for i in 0..12 {
        let category = category::ActiveModel {
            sort_order: Set(10 + i),
            ..testing::new_category(&format!("หมวด {i}"))
        };
        category.insert(&s.db).await.unwrap();
    }
    s.taps(a, "add_product").await;
    for answer in ["ข้าม", "ไข่ไก่", "110"] {
        s.types(a, answer).await;
    }
    let first = s.types(a, "แผง").await;
    let replies = quick_replies(&first[0]);
    assert_eq!(replies.len(), limits::QUICK_REPLY_ITEMS);
    assert_eq!(replies[..3], ["ไข่/นม", "ขนม/ของหวาน", "หมวด 0"]);
    assert_eq!(replies[10..], ["หมวด 8", "ดูเพิ่มเติม", "ยกเลิก"]);
    let more = quick_reply_data(&first[0])[11].expect("a postback");
    assert_eq!(more, "add_product.categories?page=1");

    let second = s.taps(a, more).await;
    assert_eq!(
        quick_replies(&second[0]),
        ["หมวด 9", "หมวด 10", "หมวด 11", "ยกเลิก"]
    );
    // A page past the last starts over
    let past = s.taps(a, "add_product.categories?page=9").await;
    assert_eq!(past, first);
    s.types(a, "หมวด 11").await;
    assert_eq!(s.step(a).await, Step::Stock);

    // Past the category, the button asks the step the seller is on again
    let again = s.taps(a, more).await;
    assert!(text(&again[0]).starts_with("ตอนนี้มีขายกี่แผงคะ"));
    assert_eq!(s.step(a).await, Step::Stock);
}

/// FR-PROD-03, FR-UI-13: a category deactivated before the product is listed is asked for
/// again, and nothing is listed meanwhile.
#[tokio::test]
async fn a_category_deactivated_before_listing_is_asked_for_again() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    s.fill(a).await;
    s.set_active(s.eggs, false).await;

    let refused = s.taps_latest(a, SUBMIT).await;
    assert_eq!(refused[0], t.category_inactive());
    assert_eq!(text(&refused[1]), text(&t.ask_product_category(None)));
    assert_eq!(quick_replies(&refused[1]), ["ขนม/ของหวาน", "ยกเลิก"]);
    assert_eq!(s.products().await, []);
    assert_eq!(s.step(a).await, Step::Category);

    let preview = s.types(a, "ขนม/ของหวาน").await;
    assert_eq!(row(&preview[1], "หมวดหมู่"), "ขนม/ของหวาน");
    s.taps_latest(a, SUBMIT).await;
    assert_eq!(s.products().await[0].category_id, s.sweets);
}

/// Photos sent but swept before the product is listed, the flow having been left too long, are
/// asked for again; nothing is listed meanwhile.
#[tokio::test]
async fn lost_photos_are_asked_for_again() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    s.fill(a).await;
    image::Entity::delete_many().exec(&s.db).await.unwrap();

    let lost = s.taps_latest(a, SUBMIT).await;
    assert_eq!(lost[0], t.product_photos_lost());
    assert_eq!(text(&lost[1]), text(&t.ask_product_photos(0, 3, true)));
    assert_eq!(s.products().await, []);
    let session = s.session(a).await.unwrap();
    assert_eq!(
        (session.step, session.draft.photos),
        (Step::Photos, Vec::new())
    );
    // Going without, it is listed
    s.types(a, "ข้าม").await;
    s.taps_latest(a, SUBMIT).await;
    assert_eq!(s.products().await.len(), 1);
    assert_eq!(s.images().await, []);
}

/// FR-UI-13: only the latest preview's buttons act; an older one asks the current step again.
#[tokio::test]
async fn only_the_latest_preview_acts() {
    let s = setup().await;
    let a = s.seller;
    s.fill(a).await;
    let old = s.revision(a).await;
    s.corrects(a, Step::Name).await;
    s.types(a, "ไข่เป็ด").await;

    let shown = s.taps(a, &format!("{SUBMIT}?r={old}")).await;
    assert_eq!(shown.len(), 2);
    assert_eq!(card_texts(&shown[1])[0], "ไข่เป็ด");
    assert_eq!(s.products().await, []);

    // While a part is being corrected, even the latest preview's button asks that part again
    let latest = s.revision(a).await;
    s.corrects(a, Step::Price).await;
    let price = s.taps(a, &format!("{SUBMIT}?r={latest}")).await;
    assert!(text(&price[0]).starts_with("ขายราคาเท่าไรคะ"));
    assert_eq!(s.step(a).await, Step::Price);
    assert_eq!(s.products().await, []);

    // Tapping "เพิ่มสินค้า" carries on where the seller is
    let resumed = s.taps(a, "add_product").await;
    assert_eq!(resumed, price);
}

/// NFR-SEC-02, FR-SHOP-10: only the owner of an approved shop adds products; a suspension
/// mid-flow ends it.
#[tokio::test]
async fn only_the_owner_of_an_approved_shop_adds_products() {
    let s = setup().await;
    let (a, t) = (s.seller, &s.templates);
    assert_eq!(
        s.taps(s.buyer, "add_product").await,
        [t.denied(Denied::NotAllowed)]
    );
    for (status, denied) in [
        (shop::Status::Pending, Denied::NotAllowed),
        (shop::Status::Rejected, Denied::NotAllowed),
        (shop::Status::Suspended, Denied::ShopSuspended),
    ] {
        s.set_status(status).await;
        assert_eq!(s.taps(a, "add_product").await, [t.denied(denied)]);
        assert_eq!(s.flow(a).await, None);
    }

    s.set_status(shop::Status::Approved).await;
    s.taps(a, "add_product").await;
    s.types(a, "ข้าม").await;
    s.set_status(shop::Status::Suspended).await;
    assert_eq!(s.types(a, "ไข่ไก่").await, [t.denied(Denied::ShopSuspended)]);
    assert_eq!(s.flow(a).await, None);

    s.set_status(shop::Status::Approved).await;
    let mut seller = user::Entity::find_by_id(a)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    seller.status = Set(user::Status::Suspended);
    seller.update(&s.db).await.unwrap();
    assert_eq!(
        s.taps(a, "add_product").await,
        [t.denied(Denied::Suspended)]
    );
    assert_eq!(s.products().await, []);
}

/// FR-PROD-03: without an active category, no product can be added, so none is started.
#[tokio::test]
async fn without_an_active_category_nothing_starts() {
    let s = setup().await;
    let a = s.seller;
    s.set_active(s.eggs, false).await;
    s.set_active(s.sweets, false).await;
    let none = s.taps(a, "add_product").await;
    assert_eq!(none, [s.templates.no_categories(), s.main_menu(a).await]);
    assert_eq!(s.flow(a).await, None);
}

/// FR-UI-14: a seller's photo that no flow takes is offered for listing, as the first photo of
/// a new product; with room for no more, the name is asked straight away.
#[tokio::test]
async fn a_sellers_photo_is_offered_for_listing() {
    let mut s = setup().await;
    let (a, line) = (s.seller, s.line.client());
    s.serves("m1", PHOTO);
    let offered = s
        .handle(a, async |turn| {
            photo_sent(turn, &s.config, &line, "m1").await
        })
        .await;
    assert_eq!(offered.len(), 1);
    assert_eq!(text(&offered[0]), text(&s.templates.listing_offered(2)));
    assert!(text(&offered[0]).starts_with("ได้รับรูปแล้วค่ะ จะลงขายสินค้านี้ใช่ไหมคะ"));
    assert_eq!(
        quick_replies(&offered[0]),
        ["ถ่ายรูป", "เลือกรูป", "ถัดไป", "ยกเลิก"]
    );
    let session = s.session(a).await.unwrap();
    let [(OwnerType::Draft, _, photo)] = s.images().await[..] else {
        panic!("one draft: {:?}", s.images().await);
    };
    assert_eq!(
        (session.step, session.draft.photos),
        (Step::Photos, vec![photo])
    );
    let name = s.types(a, "ถัดไป").await;
    assert!(text(&name[0]).starts_with("สินค้าชื่ออะไรคะ"));

    s.config.image.max_per_product = 1;
    s.serves("m2", PHOTO);
    let offered = s
        .handle(a, async |turn| {
            photo_sent(turn, &s.config, &line, "m2").await
        })
        .await;
    assert_eq!(
        offered,
        [
            s.templates.listing_offered(0),
            with_cancel(s.templates.ask_product_name(NAME_CHARS, None))
        ]
    );
    assert_eq!(s.step(a).await, Step::Name);

    // A photo that can't be kept starts nothing
    s.handle(a, async |turn| flow::cancel(turn).await).await;
    s.serves("m3", b"not a photo");
    let refused = s
        .handle(a, async |turn| {
            photo_sent(turn, &s.config, &line, "m3").await
        })
        .await;
    assert_eq!(
        text(&refused[0]),
        s.templates.image_rejected(Rejected::NotJpegOrPng)
    );
    assert_eq!(s.flow(a).await, None);
}
