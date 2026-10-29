//! "ซื้อของ" and "ค้นหา" (FR-SRCH): what buyers see of the products and shops, in what order and
//! how paged; the cards and details of products, a pre-order's ready date (FR-PROD-11); searches
//! that page through what they keep; and what an empty list says. Each event is handled in a transaction committed once it is handled.
//! The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use std::slice;

use chrono::{NaiveDate, TimeDelta};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, DbBackend, QueryTrait, Statement,
    TransactionTrait,
};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::search::{KEYWORD_CHARS, Refused, Search, SearchFlow};
use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::flow;
use crate::dispatcher::{Finished, Input};
use crate::line::flex::{Bubble, Component, FlexContainer, Hero};
use crate::line::{Body, limits};
use crate::notifications::templates::{PRODUCTS_PER_PAGE, Templates};
use crate::persistence::entity::{image, session, user};
use crate::persistence::{orm, random_id, testing};
use crate::thai::Money;
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const OTHER_BUYER: &str = "Ue5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";

/// When buyers browse: three days after the products [`testing::new_product`] makes were listed
fn now() -> DateTimeUtc {
    testing::at() + TimeDelta::days(3)
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    buyer: i64,
    /// ร้านป้าแดง, approved and open
    shop: i64,
    /// The active categories, in order: ไข่/นม and ขนม/ของหวาน. ผลไม้ follows, inactive.
    eggs: i64,
    sweets: i64,
    fruit: i64,
    /// Makes each shop's owner unique
    owners: std::cell::Cell<u32>,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
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
    let fruit = category("ผลไม้", 3, false).await;
    let config = config::sample();
    let s = Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        buyer,
        shop: 0,
        eggs,
        sweets,
        fruit,
        owners: std::cell::Cell::new(0),
    };
    let shop = s.shop("ร้านป้าแดง", shop::Status::Approved, true).await;
    Setup { shop, ..s }
}

impl Setup {
    /// A shop called `name`, `status` and open or not, of a new owner.
    async fn shop(&self, name: &str, status: shop::Status, is_open: bool) -> i64 {
        let number = self.owners.get() + 1;
        self.owners.set(number);
        let owner = testing::new_user(&format!("U-seller-{number}"))
            .insert(&self.db)
            .await
            .unwrap();
        let shop = shop::ActiveModel {
            name: Set(name.to_owned()),
            status: Set(status),
            is_open: Set(is_open),
            ..testing::new_shop(owner.id)
        };
        shop.insert(&self.db).await.unwrap().id
    }

    /// Lists `product`, made with [`testing::new_product`] and changed as it needs.
    async fn insert(&self, product: product::ActiveModel) -> i64 {
        product.insert(&self.db).await.unwrap().id
    }

    /// Lists a product called `name` with `stock` in `shop`, among the eggs.
    async fn list_in(&self, shop: i64, name: &str, stock: Option<i32>) -> i64 {
        self.insert(testing::new_product(shop, self.eggs, name, stock))
            .await
    }

    /// Lists a product of ร้านป้าแดง's.
    async fn list(&self, name: &str, stock: Option<i32>) -> i64 {
        self.list_in(self.shop, name, stock).await
    }

    /// Adds a photo to product `id`, after any it has.
    async fn add_photo(&self, id: i64, position: i16) -> Uuid {
        let photo = image::ActiveModel {
            id: Set(random_id()),
            owner_type: Set(image::OwnerType::Product),
            owner_id: Set(id),
            position: Set(position),
            content_type: Set("image/jpeg".to_owned()),
            width: Set(1),
            height: Set(1),
            size_bytes: Set(1),
            data: Set(vec![0]),
            created_at: Set(testing::at()),
        };
        photo.insert(&self.db).await.unwrap().id
    }

    /// Handles an event of user `user_id`'s at `now` with `handle`, as the worker does.
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
        let Finished { messages, .. } = turn.finish(messages);
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
            tapped(turn, &self.config, &postback).await
        })
        .await
    }

    /// The buyer taps a button sending `data`.
    async fn taps(&self, data: &str) -> Vec<Message> {
        self.taps_at(self.buyer, data, now()).await
    }

    /// The one message the buyer gets for tapping a button sending `data`.
    async fn tap(&self, data: &str) -> Message {
        let mut messages = self.taps(data).await;
        assert_eq!(messages.len(), 1, "{messages:?}");
        messages.remove(0)
    }

    /// User `user_id` taps "ค้นหา" at `now`.
    async fn starts_search(&self, user_id: i64, now: DateTimeUtc) -> Vec<Message> {
        self.handle_at(user_id, now, async |turn| search::start(turn).await)
            .await
    }

    /// User `user_id` sends `input` at `now` to the search they started; `None` if it doesn't
    /// take it.
    async fn sends(&self, user_id: i64, input: Input, now: DateTimeUtc) -> Option<Vec<Message>> {
        let mut answered = None;
        self.handle_at(user_id, now, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<SearchFlow>());
            let session = session.expect("searching");
            answered = search::answer(turn, &self.config, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    /// The buyer taps "ค้นหา" and types `keyword`: what they get for it.
    async fn search(&self, keyword: &str) -> Vec<Message> {
        self.starts_search(self.buyer, now()).await;
        let input = Input::Text(keyword.to_owned());
        self.sends(self.buyer, input, now())
            .await
            .expect("an answer")
    }

    /// The names of the products page `page` of `listing` shows at `now`, in order.
    async fn names_at(&self, listing: Listing<'_>, page: usize, now: DateTimeUtc) -> Vec<String> {
        listing::page(&self.db, listing, now, page)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|(product, _)| product.name)
            .collect()
    }

    async fn names(&self, listing: Listing<'_>) -> Vec<String> {
        self.names_at(listing, 0, now()).await
    }

    /// The names of the products a search for `keyword` finds.
    async fn found(&self, keyword: &str) -> Vec<String> {
        let search = Search::parse(keyword).unwrap();
        self.names(Listing::Search(&search)).await
    }

    /// The flow user `user_id` is in, if any.
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

    fn placeholder(&self) -> String {
        images::placeholder_url(&self.config.app.base_url).to_string()
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

fn alt_text(message: &Message) -> &str {
    match &message.body {
        Body::Flex { alt_text, .. } => alt_text,
        body => panic!("not a card: {body:?}"),
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

/// The product cards of a page of a list, and the postback data of its "ดูเพิ่มเติม" if any.
fn page(message: &Message) -> (Vec<&Bubble>, Option<String>) {
    let mut cards = cards(message);
    let more = cards.last().and_then(|last| {
        let body = last.body.as_ref()?;
        match body.contents.as_slice() {
            [Component::Button(button)] => match &button.action {
                Action::Postback { label, data, .. } if *label == labels::MORE => {
                    Some(data.clone())
                }
                _ => None,
            },
            _ => None,
        }
    });
    if more.is_some() {
        cards.pop();
    }
    (cards, more)
}

/// The texts of `card`'s body, in order: a product's name, price, shop and availability.
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

/// The names of the products on the cards of a page of a list.
fn names(message: &Message) -> Vec<String> {
    let (cards, _) = page(message);
    cards.iter().map(|card| texts(card)[0].clone()).collect()
}

/// The label and postback data of each button of `card`.
fn buttons(card: &Bubble) -> Vec<(String, String)> {
    let Some(footer) = &card.footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|button| match button {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => (label.as_str().to_owned(), data.clone()),
                action => panic!("not a postback: {action:?}"),
            },
            component => panic!("not a button: {component:?}"),
        })
        .collect()
}

fn labels_of(card: &Bubble) -> Vec<String> {
    buttons(card).into_iter().map(|(label, _)| label).collect()
}

/// The postback data of a tap on `card` itself.
fn tap_data(card: &Bubble) -> &str {
    match &card.action {
        Some(Action::Postback { data, .. }) => data,
        action => panic!("not a postback: {action:?}"),
    }
}

/// The link of the photo across the top of `card`.
fn hero(card: &Bubble) -> &str {
    match &card.hero {
        Some(Hero::Image(image)) => &image.url,
        hero => panic!("no photo: {hero:?}"),
    }
}

/// Acceptance, FR-SRCH-04: a keyword matches anywhere inside a product's name, description,
/// category name or shop name, as Thai has no spaces between words; Latin letters whatever their
/// case; every word of a keyword of several; and what is typed as written, never as a pattern.
#[tokio::test]
async fn a_search_finds_what_contains_the_keyword() {
    let s = setup().await;
    let uncle = s.shop("ร้านลุงชม", shop::Status::Approved, true).await;
    s.insert(testing::new_product(s.shop, s.sweets, "ขนมครก", Some(30)))
        .await;
    s.insert(product::ActiveModel {
        description: Set(Some("ไข่สดจากฟาร์ม เก็บทุกเช้า".to_owned())),
        ..testing::new_product(uncle, s.eggs, "ไข่ไก่เบอร์ 0", Some(3))
    })
    .await;
    s.list_in(uncle, "Fresh Milk", None).await;
    s.list("ลดราคา 50%", None).await;
    s.list("น้ำปลา_ตราชั่ง", None).await;

    // Acceptance: inside a word
    assert_eq!(s.found("ครก").await, ["ขนมครก"]);
    // The description, the category and the shop
    assert_eq!(s.found("ฟาร์ม").await, ["ไข่ไก่เบอร์ 0"]);
    assert_eq!(s.found("ของหวาน").await, ["ขนมครก"]);
    let uncles = s.found("ลุงชม").await;
    assert_eq!(uncles.len(), 2, "{uncles:?}");
    assert!(uncles.contains(&"Fresh Milk".to_owned()), "{uncles:?}");
    // Whatever the case
    assert_eq!(s.found("fresh MILK").await, ["Fresh Milk"]);
    // Every word, each anywhere; extra spaces count as one
    assert_eq!(s.found("  ไข่   ฟาร์ม ").await, ["ไข่ไก่เบอร์ 0"]);
    assert_eq!(s.found("ครก ลุงชม").await, Vec::<String>::new());
    // As written: `%` and `_` are not wildcards
    assert_eq!(s.found("%").await, ["ลดราคา 50%"]);
    assert_eq!(s.found("_").await, ["น้ำปลา_ตราชั่ง"]);
    assert_eq!(s.found("50_").await, Vec::<String>::new());
    assert_eq!(s.found("\\").await, Vec::<String>::new());
}

/// FR-SRCH-05: the AI assistant's search finds what matches any of its keywords, each as a
/// typed one would, within a category, up to a price and from shops that deliver; what it
/// adds reads as nothing from a search kept before it could.
#[tokio::test]
async fn an_assistants_search_takes_other_words_and_filters() {
    let s = setup().await;
    let pickup_only = shop::ActiveModel {
        name: Set("ร้านลุงชม".to_owned()),
        offers_delivery: Set(false),
        ..testing::new_shop(testing::new_user("U-uncle").insert(&s.db).await.unwrap().id)
    };
    let uncle = pickup_only.insert(&s.db).await.unwrap().id;
    s.insert(product::ActiveModel {
        price: Set(Money::from_baht(45)),
        ..testing::new_product(s.shop, s.eggs, "ผักบุ้ง", Some(10))
    })
    .await;
    s.insert(product::ActiveModel {
        price: Set(Money::from_baht(60)),
        ..testing::new_product(s.shop, s.eggs, "คะน้า สด", Some(10))
    })
    .await;
    s.insert(product::ActiveModel {
        price: Set(Money::from_baht(30)),
        ..testing::new_product(uncle, s.eggs, "ผักกาด", Some(10))
    })
    .await;
    s.insert(testing::new_product(s.shop, s.sweets, "ขนมผักกาด", Some(10)))
        .await;
    let found = async |search: Search| {
        let mut names = s.names(Listing::Search(&search)).await;
        names.sort();
        names
    };

    // Any of the words, each matched as a typed keyword is, repeats and blanks left out
    let greens = Search::any_of(["ผักบุ้ง", " ", "คะน้า   สด", "ผักบุ้ง"]).unwrap();
    assert_eq!(greens.keyword(), "ผักบุ้ง");
    assert_eq!(found(greens.clone()).await, ["คะน้า สด", "ผักบุ้ง"]);
    assert_eq!(Search::any_of(["", " "]), Err(Refused::Empty));

    let vegetables = Search::any_of(["ผัก"]).unwrap();
    assert_eq!(
        found(vegetables.clone()).await,
        ["ขนมผักกาด", "ผักกาด", "ผักบุ้ง"]
    );
    assert_eq!(
        found(vegetables.clone().in_category(s.eggs)).await,
        ["ผักกาด", "ผักบุ้ง"]
    );
    // The price per unit, up to and including
    let cheap = vegetables.clone().at_most(Money::from_baht(45));
    assert_eq!(found(cheap.clone()).await, ["ผักกาด", "ผักบุ้ง"]);
    assert_eq!(found(cheap.delivered()).await, ["ผักบุ้ง"]);
    let greens = greens.in_category(s.eggs).at_most(Money::from_baht(50));
    assert_eq!(found(greens.delivered()).await, ["ผักบุ้ง"]);

    // A search kept before the filters still reads, as the keyword alone
    let kept: Search = serde_json::from_value(serde_json::json!({ "keyword": "ผัก" })).unwrap();
    assert_eq!(kept, vegetables);
    assert_eq!(
        serde_json::to_value(&vegetables).unwrap(),
        serde_json::json!({ "keyword": "ผัก" })
    );
}

/// The AI assistant's `search_shops`: the shops buyers see whose name or description holds
/// every word, whatever the case, open ones first, and what is typed as written.
#[tokio::test]
async fn shops_are_found_by_their_name_or_description() {
    let s = setup().await;
    s.shop("ร้านลุงชม Coffee", shop::Status::Approved, false)
        .await;
    s.shop("ครัวคุณแม่", shop::Status::Approved, true).await;
    s.shop("ร้านป้าแดงสาขา 2", shop::Status::Pending, true).await;
    let found = async |keyword: &str| -> Vec<String> {
        listing::find_shops(&s.db, keyword, 0)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|shop| shop.name)
            .collect()
    };

    assert_eq!(found("ร้าน").await, ["ร้านป้าแดง", "ร้านลุงชม Coffee"]);
    assert_eq!(found("coffee").await, ["ร้านลุงชม Coffee"]);
    // Each one's description is "ไข่ไก่ ผักสวนครัว"
    assert_eq!(
        found("ผักสวน ครัว").await,
        ["ครัวคุณแม่", "ร้านป้าแดง", "ร้านลุงชม Coffee"]
    );
    assert_eq!(found("ป้าแดง ครัว").await, ["ร้านป้าแดง"]);
    assert_eq!(found("%").await, Vec::<String>::new());
}

/// Acceptance, FR-SRCH-02: hidden, held, removed, draft and deleted products, and the products
/// of shops that are pending, held, suspended, rejected or closed, or whose owner is suspended
/// (FR-ADM-04), are in no list, and their details and shops are not found. A closed-for-now
/// shop's products are listed.
#[tokio::test]
async fn buyers_never_see_what_is_hidden_held_or_suspended() {
    let s = setup().await;
    let shown = s.list("ไข่ไก่ ลงขาย", Some(5)).await;
    for status in [
        product::Status::Hidden,
        product::Status::Held,
        product::Status::Removed,
        product::Status::Draft,
        product::Status::Deleted,
    ] {
        s.insert(product::ActiveModel {
            status: Set(status),
            created_at: Set(now()),
            ..testing::new_product(s.shop, s.eggs, &format!("ไข่ไก่ {status:?}"), Some(5))
        })
        .await;
    }
    let mut hidden_shops = Vec::new();
    let mut hidden_products = Vec::new();
    for status in [
        shop::Status::Pending,
        shop::Status::Held,
        shop::Status::Suspended,
        shop::Status::Rejected,
        shop::Status::Closed,
    ] {
        let shop = s.shop(&format!("ร้าน{status:?}"), status, true).await;
        hidden_shops.push(shop);
        let product = s.insert(product::ActiveModel {
            created_at: Set(now()),
            ..testing::new_product(shop, s.eggs, &format!("ไข่ไก่ร้าน {status:?}"), Some(5))
        });
        hidden_products.push(product.await);
    }
    let of_suspended = s
        .shop("ร้านเจ้าของถูกระงับ", shop::Status::Approved, true)
        .await;
    let owner = shop::Entity::find_by_id(of_suspended)
        .one(&s.db)
        .await
        .unwrap()
        .unwrap()
        .owner_id;
    let suspended = user::ActiveModel {
        id: Set(owner),
        status: Set(user::Status::Suspended),
        ..Default::default()
    };
    suspended.update(&s.db).await.unwrap();
    hidden_shops.push(of_suspended);
    let product = s.insert(product::ActiveModel {
        created_at: Set(now()),
        ..testing::new_product(of_suspended, s.eggs, "ไข่ไก่ร้านเจ้าของถูกระงับ", Some(5))
    });
    hidden_products.push(product.await);
    let closed = s.shop("ร้านปิดชั่วคราว", shop::Status::Approved, false).await;
    s.list_in(closed, "ไข่ไก่ ร้านปิด", Some(5)).await;
    let listed = ["ไข่ไก่ ลงขาย", "ไข่ไก่ ร้านปิด"];

    assert_eq!(s.names(Listing::Category(s.eggs)).await, listed);
    assert_eq!(s.found("ไข่ไก่").await, listed);
    assert_eq!(s.names(Listing::Shop(s.shop)).await, ["ไข่ไก่ ลงขาย"]);
    let new_today = s.names(Listing::NewToday).await;
    assert_eq!(new_today, Vec::<String>::new());
    let category = s.tap(&format!("browse?category={}", s.eggs)).await;
    assert_eq!(names(&category), listed);

    let not_found = s.templates.denied(Denied::NotFound);
    for id in hidden_products {
        assert_eq!(
            s.taps(&format!("browse.product?product={id}")).await,
            slice::from_ref(&not_found)
        );
    }
    for id in hidden_shops {
        assert_eq!(
            s.taps(&format!("browse.front?shop={id}")).await,
            slice::from_ref(&not_found)
        );
        assert_eq!(
            s.taps(&format!("browse.shop?shop={id}")).await,
            slice::from_ref(&not_found)
        );
    }
    let hidden = product::Entity::find()
        .filter(product::Column::Status.eq(product::Status::Hidden))
        .one(&s.db)
        .await
        .unwrap()
        .unwrap();
    let tapped = s
        .taps(&format!("browse.product?product={}", hidden.id))
        .await;
    assert_eq!(tapped, slice::from_ref(&not_found));
    let details = s.tap(&format!("browse.product?product={shown}")).await;
    assert_eq!(alt_text(&details), "ไข่ไก่ ลงขาย");
    let shops = s.tap("browse.shops").await;
    let shops: Vec<_> = cards(&shops)
        .iter()
        .map(|card| texts(card)[0].clone())
        .collect();
    assert_eq!(shops, ["ร้านป้าแดง", "ร้านปิดชั่วคราว"]);

    // An inactive category is not browsed, as "ซื้อของ" no longer offers it
    let fruit = s.taps(&format!("browse?category={}", s.fruit)).await;
    assert_eq!(fruit, [not_found]);
}

/// FR-SRCH-02: open shops' products first, then those in stock, then the most recently changed.
#[tokio::test]
async fn lists_show_open_shops_then_in_stock_then_newest_first() {
    let s = setup().await;
    let closed = s.shop("ร้านลุงชม", shop::Status::Approved, false).await;
    let at = |hours: i64| testing::at() + TimeDelta::hours(hours);
    for (shop, name, stock, updated) in [
        (closed, "ปิด มีของ ใหม่", Some(5), at(3)),
        (s.shop, "เปิด หมด ใหม่", Some(0), at(3)),
        (s.shop, "เปิด มีของ เก่า", Some(5), at(1)),
        (s.shop, "เปิด ทำตามสั่ง ใหม่", None, at(2)),
        (closed, "ปิด หมด", Some(0), at(4)),
        (s.shop, "เปิด หมด เก่า", Some(0), at(1)),
    ] {
        s.insert(product::ActiveModel {
            updated_at: Set(updated),
            ..testing::new_product(shop, s.eggs, name, stock)
        })
        .await;
    }
    let expected = [
        "เปิด ทำตามสั่ง ใหม่",
        "เปิด มีของ เก่า",
        "เปิด หมด ใหม่",
        "เปิด หมด เก่า",
        "ปิด มีของ ใหม่",
        "ปิด หมด",
    ];
    assert_eq!(s.names(Listing::Category(s.eggs)).await, expected);
    assert_eq!(s.found("ป").await, expected);
}

/// FR-SRCH-03: ten products a page, then "ดูเพิ่มเติม" for the next; a page that no longer exists
/// starts over.
#[tokio::test]
async fn long_lists_are_paged_ten_at_a_time() {
    let s = setup().await;
    for number in 1..=11 {
        s.insert(product::ActiveModel {
            updated_at: Set(testing::at() + TimeDelta::minutes(number)),
            ..testing::new_product(s.shop, s.eggs, &format!("ไข่ไก่ {number}"), Some(5))
        })
        .await;
    }
    let category = format!("browse?category={}", s.eggs);
    let first = s.tap(&category).await;
    let (cards, more) = page(&first);
    assert_eq!(cards.len(), PRODUCTS_PER_PAGE);
    assert_eq!(texts(cards[0])[0], "ไข่ไก่ 11");
    let more = more.expect("ดูเพิ่มเติม");
    assert_eq!(more, format!("browse?category={}&page=1", s.eggs));
    assert_eq!(alt_text(&first), "สินค้าหมวด ไข่/นม");
    assert_eq!(quick_replies(&first), ["ซื้อของ", "ค้นหา", "เมนู"]);

    let second = s.tap(&more).await;
    assert_eq!(page(&second).1, None);
    assert_eq!(names(&second), ["ไข่ไก่ 1"]);

    // From a button sent before products went
    let gone = s.tap(&format!("browse?category={}&page=7", s.eggs)).await;
    assert_eq!(names(&gone), names(&first));

    // A shop's list pages likewise
    let shop = s.tap(&format!("browse.shop?shop={}", s.shop)).await;
    assert_eq!(alt_text(&shop), "สินค้าของร้านป้าแดง");
    assert_eq!(
        page(&shop).1.as_deref(),
        Some(format!("browse.shop?shop={}&page=1", s.shop).as_str())
    );
}

/// FR-SRCH-09: "ของใหม่วันนี้" lists what was listed or restocked within the last 24 hours.
#[tokio::test]
async fn new_today_lists_what_was_listed_or_restocked_in_the_last_day() {
    let s = setup().await;
    let ago = |hours: i64| now() - TimeDelta::hours(hours);
    for (name, created, restocked) in [
        ("ลงเมื่อ 23 ชั่วโมงก่อน", ago(23), None),
        ("ลงเมื่อ 25 ชั่วโมงก่อน", ago(25), None),
        ("เติมของเมื่อชั่วโมงก่อน", ago(72), Some(ago(1))),
        ("เติมของเมื่อวานซืน", ago(72), Some(ago(48))),
    ] {
        s.insert(product::ActiveModel {
            created_at: Set(created),
            updated_at: Set(restocked.unwrap_or(created)),
            restocked_at: Set(restocked),
            ..testing::new_product(s.shop, s.eggs, name, Some(5))
        })
        .await;
    }
    let new_today = s.names(Listing::NewToday).await;
    assert_eq!(new_today, ["เติมของเมื่อชั่วโมงก่อน", "ลงเมื่อ 23 ชั่วโมงก่อน"]);
    let tapped = s.tap("browse.new").await;
    assert_eq!(names(&tapped), new_today);
    assert_eq!(alt_text(&tapped), "ของใหม่วันนี้");

    // A day later, nothing is new
    let tomorrow = now() + TimeDelta::days(1);
    let none = s.names_at(Listing::NewToday, 0, tomorrow).await;
    assert_eq!(none, Vec::<String>::new());
}

/// FR-SRCH-01: "ซื้อของ" offers "ของใหม่วันนี้" and "ร้านค้าทั้งหมด", then the active categories,
/// ten a page.
#[tokio::test]
async fn buying_offers_new_today_and_every_shop_before_the_categories() {
    let s = setup().await;
    let first = s.tap("buy").await;
    assert_eq!(
        quick_replies(&first),
        ["ของใหม่วันนี้", "ร้านค้าทั้งหมด", "ไข่/นม", "ขนม/ของหวาน"]
    );
    let data = quick_reply_data(&first);
    assert_eq!(data[0], Some("browse.new"));
    assert_eq!(data[1], Some("browse.shops"));
    assert_eq!(
        data[2],
        Some(format!("browse?category={}", s.eggs).as_str())
    );

    for number in 1..=9 {
        let category = category::ActiveModel {
            sort_order: Set(10 + number),
            ..testing::new_category(&format!("หมวด {number}"))
        };
        category.insert(&s.db).await.unwrap();
    }
    let first = s.tap("buy").await;
    let labels = quick_replies(&first);
    assert_eq!(labels.len(), limits::QUICK_REPLY_ITEMS);
    assert_eq!(labels[..3], ["ของใหม่วันนี้", "ร้านค้าทั้งหมด", "ไข่/นม"]);
    assert_eq!(labels[12], "ดูเพิ่มเติม");
    assert_eq!(quick_reply_data(&first)[12], Some("buy?page=1"));
    let second = s.tap("buy?page=1").await;
    assert_eq!(
        quick_replies(&second),
        ["ของใหม่วันนี้", "ร้านค้าทั้งหมด", "หมวด 9"]
    );
}

/// FR-SRCH-07: a card shows the product's photo, or the placeholder, its name, price per unit,
/// shop and availability, with "ใส่ตะกร้า" only while it can be ordered, and "ดูร้าน"; a tap
/// on it shows its details.
#[tokio::test]
async fn a_card_shows_what_buyers_need_to_choose() {
    let s = setup().await;
    let closed = s.shop("ร้านลุงชม", shop::Status::Approved, false).await;
    let at = |minutes: i64| testing::at() + TimeDelta::minutes(minutes);
    let mut ids = Vec::new();
    for (shop, name, stock, updated) in [
        (s.shop, "ไข่ไก่เบอร์ 2", Some(5), at(4)),
        (s.shop, "ขนมครก", None, at(3)),
        (s.shop, "ไข่เป็ด", Some(0), at(2)),
        (closed, "ไข่ไก่เบอร์ 0", Some(5), at(1)),
    ] {
        let product = s.insert(product::ActiveModel {
            price: Set(Money::from_satang(11_050)),
            updated_at: Set(updated),
            ..testing::new_product(shop, s.eggs, name, stock)
        });
        ids.push(product.await);
    }
    let first = s.add_photo(ids[0], 0).await;
    s.add_photo(ids[0], 1).await;

    let list = s.tap(&format!("browse?category={}", s.eggs)).await;
    let (listed, more) = page(&list);
    assert_eq!(more, None);
    let shown: Vec<_> = listed.iter().map(|card| texts(card)).collect();
    assert_eq!(
        shown,
        [
            ["ไข่ไก่เบอร์ 2", "฿110.50 / แผง", "ร้านป้าแดง", "เหลือ 5"],
            ["ขนมครก", "฿110.50 / แผง", "ร้านป้าแดง", "สั่งได้"],
            ["ไข่เป็ด", "฿110.50 / แผง", "ร้านป้าแดง", "หมด"],
            ["ไข่ไก่เบอร์ 0", "฿110.50 / แผง", "ร้านลุงชม", "ร้านปิด"],
        ]
    );
    assert_eq!(hero(listed[0]), s.photo_url(first));
    assert_eq!(hero(listed[1]), s.placeholder());
    let add = |id: i64| {
        (
            labels::ADD_TO_CART.as_str().to_owned(),
            format!("cart.add?product={id}"),
        )
    };
    let view = |shop: i64| {
        (
            labels::VIEW_SHOP.as_str().to_owned(),
            format!("browse.front?shop={shop}"),
        )
    };
    assert_eq!(buttons(listed[0]), [add(ids[0]), view(s.shop)]);
    assert_eq!(buttons(listed[1]), [add(ids[1]), view(s.shop)]);
    // Sold out, or its shop closed: no "ใส่ตะกร้า" (FR-PROD-06)
    assert_eq!(buttons(listed[2]), [view(s.shop)]);
    assert_eq!(buttons(listed[3]), [view(closed)]);
    for (card, id) in listed.iter().zip(&ids) {
        assert_eq!(tap_data(card), format!("browse.product?product={id}"));
    }

    // "ดูร้าน": the shop's card, whose "ดูสินค้า" lists its products
    let front = s.tap(&format!("browse.front?shop={}", s.shop)).await;
    assert_eq!(alt_text(&front), "ร้านป้าแดง");
    let [card] = cards(&front)[..] else {
        panic!("not one card");
    };
    assert_eq!(
        buttons(card),
        [("ดูสินค้า".to_owned(), format!("browse.shop?shop={}", s.shop))]
    );
    assert_eq!(quick_replies(&front), ["ซื้อของ", "ค้นหา", "เมนู"]);
}

/// FR-PROD-11: a pre-order's card and details say when it is ready, until that day comes; from
/// then on, it is an ordinary product.
#[tokio::test]
async fn a_pre_order_says_when_it_is_ready_until_it_is() {
    let s = setup().await;
    // now() is 25 ต.ค. 2569
    let mooncakes = s
        .insert(product::ActiveModel {
            ready_on: Set(NaiveDate::from_ymd_opt(2026, 11, 5)),
            ..testing::new_product(s.shop, s.sweets, "ขนมไหว้พระจันทร์", Some(20))
        })
        .await;
    let ready = "สั่งล่วงหน้า · พร้อม 5 พ.ย. 2569";
    let category = format!("browse?category={}", s.sweets);
    let list = s.tap(&category).await;
    let (listed, _) = page(&list);
    assert_eq!(
        texts(listed[0]),
        ["ขนมไหว้พระจันทร์", "฿110 / แผง", "ร้านป้าแดง", "เหลือ 20", ready]
    );
    assert_eq!(labels_of(listed[0]), ["ใส่ตะกร้า", "ดูร้าน"]);
    let details = s.tap(&format!("browse.product?product={mooncakes}")).await;
    assert_eq!(
        texts(cards(&details)[0])[..4],
        ["ขนมไหว้พระจันทร์", "฿110 / แผง", "เหลือ 20", ready]
    );

    let the_day = testing::at() + TimeDelta::days(14);
    let list = s.taps_at(s.buyer, &category, the_day).await;
    let (listed, _) = page(&list[0]);
    assert_eq!(
        texts(listed[0]),
        ["ขนมไหว้พระจันทร์", "฿110 / แผง", "ร้านป้าแดง", "เหลือ 20"]
    );
}

/// FR-SRCH-07: a product's details show every photo, its description and its shop, with
/// "ใส่ตะกร้า" while it can be ordered, "ดูร้าน" and "แจ้งปัญหา".
#[tokio::test]
async fn a_products_details_show_every_photo_its_description_and_shop() {
    let s = setup().await;
    let eggs = s
        .insert(product::ActiveModel {
            description: Set(Some("ไข่สดจากฟาร์ม".to_owned())),
            ..testing::new_product(s.shop, s.eggs, "ไข่ไก่เบอร์ 2", Some(8))
        })
        .await;
    let photos = [
        s.add_photo(eggs, 0).await,
        s.add_photo(eggs, 1).await,
        s.add_photo(eggs, 2).await,
    ];

    let details = s.tap(&format!("browse.product?product={eggs}")).await;
    assert_eq!(alt_text(&details), "ไข่ไก่เบอร์ 2");
    let shown = cards(&details);
    let heroes: Vec<_> = shown.iter().map(|card| hero(card).to_owned()).collect();
    let urls: Vec<_> = photos.iter().map(|&id| s.photo_url(id)).collect();
    assert_eq!(heroes, urls);
    assert_eq!(
        texts(shown[0]),
        [
            "ไข่ไก่เบอร์ 2",
            "฿110 / แผง",
            "เหลือ 8",
            "ไข่สดจากฟาร์ม",
            "ร้าน",
            "ร้านป้าแดง"
        ]
    );
    assert_eq!(labels_of(shown[0]), ["ใส่ตะกร้า", "ดูร้าน", "แจ้งปัญหา"]);
    assert_eq!(buttons(shown[0])[2].1, format!("report?p={eggs}"));
    assert_eq!(quick_replies(&details), ["ซื้อของ", "ค้นหา", "เมนู"]);

    // Without photos, the placeholder; sold out, no "ใส่ตะกร้า"
    let sold_out = s.list("ไข่เป็ด", Some(0)).await;
    let details = s.tap(&format!("browse.product?product={sold_out}")).await;
    let [card] = cards(&details)[..] else {
        panic!("not one card");
    };
    assert_eq!(hero(card), s.placeholder());
    assert_eq!(texts(card)[2], "หมด");
    assert_eq!(labels_of(card), ["ดูร้าน", "แจ้งปัญหา"]);
}

/// FR-SRCH-06: "ร้านค้าทั้งหมด" lists the shops buyers see, open ones first, as their cards.
#[tokio::test]
async fn every_shop_is_listed_open_ones_first() {
    let s = setup().await;
    s.shop("ร้านลุงชม", shop::Status::Approved, false).await;
    s.shop("ร้านกาแฟ", shop::Status::Approved, true).await;
    s.shop("ร้านรออนุมัติ", shop::Status::Pending, true).await;
    let shops = s.tap("browse.shops").await;
    assert_eq!(alt_text(&shops), "ร้านค้าทั้งหมด");
    let (cards, more) = page(&shops);
    assert_eq!(more, None);
    let shown: Vec<_> = cards.iter().map(|card| texts(card)[..2].to_vec()).collect();
    assert_eq!(
        shown,
        [
            ["ร้านกาแฟ", "เปิดอยู่"],
            ["ร้านป้าแดง", "เปิดอยู่"],
            ["ร้านลุงชม", "ร้านปิด"]
        ]
    );
    assert_eq!(quick_replies(&shops), ["ซื้อของ", "ค้นหา", "เมนู"]);
}

/// FR-SRCH-04, FR-SRCH-03: "ค้นหา" asks for a keyword, and a search longer than a page keeps it
/// for "ดูเพิ่มเติม", for the user who searched, until it expires or is cancelled.
#[tokio::test]
async fn a_search_pages_through_what_it_kept() {
    let s = setup().await;
    for number in 1..=11 {
        s.insert(product::ActiveModel {
            updated_at: Set(testing::at() + TimeDelta::minutes(number)),
            ..testing::new_product(s.shop, s.eggs, &format!("ไข่ไก่ {number}"), Some(5))
        })
        .await;
    }
    s.list("ขนมครก", Some(5)).await;

    let asked = s.starts_search(s.buyer, now()).await;
    assert_eq!(asked.len(), 1);
    assert_eq!(text(&asked[0]), text(&s.templates.ask_keyword()));
    assert_eq!(quick_replies(&asked[0]), ["ยกเลิก"]);
    assert_eq!(s.flow(s.buyer).await.as_deref(), Some("search"));

    let input = Input::Text("ไข่ไก่".to_owned());
    let found = s.sends(s.buyer, input, now()).await.expect("an answer");
    assert_eq!(s.flow(s.buyer).await, None);
    let [found] = &found[..] else {
        panic!("not one message: {found:?}");
    };
    assert_eq!(alt_text(found), "ผลการค้นหา \"ไข่ไก่\"");
    assert_eq!(names(found).len(), PRODUCTS_PER_PAGE);
    let more = page(found).1.expect("ดูเพิ่มเติม");
    let kept = Postback::parse(&more).unwrap();
    assert_eq!(kept.action(), search::MORE);
    assert_eq!(kept.int("page"), Some(1));

    // Tapped as often as wanted, while it is kept
    for _ in 0..2 {
        let second = s.tap(&more).await;
        assert_eq!(names(&second), ["ไข่ไก่ 1"]);
        assert_eq!(page(&second).1, None);
    }

    // Not another user's, nor once it expired: the keyword is asked for again
    let other = testing::new_user(OTHER_BUYER)
        .insert(&s.db)
        .await
        .unwrap()
        .id;
    let expired = s.taps_at(other, &more, now()).await;
    assert_eq!(expired.len(), 1);
    assert_eq!(text(&expired[0]), text(&s.templates.search_expired()));
    assert_eq!(s.flow(other).await.as_deref(), Some("search"));
    let later = now() + TimeDelta::minutes(61);
    let expired = s.taps_at(s.buyer, &more, later).await;
    assert_eq!(text(&expired[0]), text(&s.templates.search_expired()));

    // Nor once the user cancelled (FR-UI-04)
    let found = s.search("ไข่ไก่").await;
    let more = page(&found[0]).1.expect("ดูเพิ่มเติม");
    s.handle_at(s.buyer, now(), async |turn| flow::cancel(turn).await)
        .await;
    let expired = s.taps(&more).await;
    assert_eq!(text(&expired[0]), text(&s.templates.search_expired()));

    // A search that fits a page keeps nothing
    let found = s.search("ครก").await;
    assert_eq!(names(&found[0]), ["ขนมครก"]);
    assert_eq!(page(&found[0]).1, None);
}

/// FR-SRCH-04: the keyword is asked for again until there is one that fits, and only text
/// answers it.
#[tokio::test]
async fn the_keyword_is_asked_for_until_one_fits() {
    let s = setup().await;
    assert_eq!(Search::parse(" \n "), Err(Refused::Empty));
    let longest = "ข".repeat(KEYWORD_CHARS);
    assert_eq!(Search::parse(&longest).unwrap().keyword(), longest);
    let too_long = "ข".repeat(KEYWORD_CHARS + 1);
    assert_eq!(
        Search::parse(&too_long),
        Err(Refused::TooLong(KEYWORD_CHARS))
    );
    assert_eq!(Search::parse(" ไข่  ไก่ ").unwrap().keyword(), "ไข่ ไก่");

    s.starts_search(s.buyer, now()).await;
    let blank = Input::Text("   ".to_owned());
    let again = s.sends(s.buyer, blank, now()).await.expect("an answer");
    assert_eq!(text(&again[0]), text(&s.templates.ask_keyword()));
    assert_eq!(quick_replies(&again[0]), ["ยกเลิก"]);
    let long = Input::Text(too_long);
    let again = s.sends(s.buyer, long, now()).await.expect("an answer");
    assert_eq!(
        text(&again[0]),
        text(&s.templates.text_too_long(KEYWORD_CHARS))
    );
    let sticker = Input::Unsupported;
    assert_eq!(s.sends(s.buyer, sticker, now()).await, None);
    assert_eq!(s.flow(s.buyer).await.as_deref(), Some("search"));
}

/// FR-SRCH-08: when nothing matches, the reply says so and suggests where else to look.
#[tokio::test]
async fn nothing_found_says_so_and_suggests_where_to_look() {
    let s = setup().await;
    let found = s.search("ทุเรียน").await;
    let [none] = &found[..] else {
        panic!("not one message: {found:?}");
    };
    let expected = s
        .templates
        .nothing_found(Browsed::Search("ทุเรียน"), Vec::new());
    assert_eq!(text(none), text(&expected));
    assert_eq!(
        quick_replies(none),
        [
            "ค้นหาใหม่",
            "ของใหม่วันนี้",
            "ร้านค้าทั้งหมด",
            "ไข่/นม",
            "ขนม/ของหวาน",
            "เมนู"
        ]
    );
    assert_eq!(quick_reply_data(none)[0], Some("search"));

    // An empty category doesn't suggest itself; an empty "ของใหม่วันนี้" neither
    let sweets = s.tap(&format!("browse?category={}", s.sweets)).await;
    assert_eq!(
        text(&sweets),
        text(
            &s.templates
                .nothing_found(Browsed::Category("ขนม/ของหวาน"), Vec::new())
        )
    );
    assert_eq!(
        quick_replies(&sweets),
        ["ค้นหาใหม่", "ของใหม่วันนี้", "ร้านค้าทั้งหมด", "ไข่/นม", "เมนู"]
    );
    let new_today = s.tap("browse.new").await;
    assert_eq!(
        quick_replies(&new_today),
        ["ค้นหาใหม่", "ร้านค้าทั้งหมด", "ไข่/นม", "ขนม/ของหวาน", "เมนู"]
    );
    let shop = s.tap(&format!("browse.shop?shop={}", s.shop)).await;
    assert_eq!(
        text(&shop),
        text(
            &s.templates
                .nothing_found(Browsed::Shop("ร้านป้าแดง"), Vec::new())
        )
    );
}

/// Buttons that name nothing, or something that isn't there, find nothing.
#[tokio::test]
async fn buttons_that_name_nothing_find_nothing() {
    let s = setup().await;
    let not_found = s.templates.denied(Denied::NotFound);
    for data in [
        "browse",
        "browse.shop",
        "browse.product",
        "browse.front",
        "browse.product?product=999",
        "browse?category=999",
        "browse.unknown",
    ] {
        assert_eq!(s.taps(data).await, slice::from_ref(&not_found), "{data}");
    }
}

/// NFR-PERF-05: at the target volume of 300 shops and 10,000 products (NFR-CAP-01), a search
/// takes at most 300 ms of database time, without an index the models can't declare (MIG-01).
/// Alone on 2 CPUs, as the target server has (NFR-CAP-02), it takes under 100.
#[tokio::test]
#[ignore = "times the database, so run it alone: cargo test a_search_takes -- --ignored"]
async fn a_search_takes_at_most_300_ms_at_the_target_volume() {
    let s = setup().await;
    let words = [
        "ไข่ไก่",
        "ขนมครก",
        "ผักบุ้ง",
        "หมูปิ้ง",
        "ข้าวเหนียว",
        "น้ำพริก",
        "กล้วยทอด",
        "ส้มตำ",
        "ปลาทู",
        "กาแฟ",
    ];
    let mut shops = Vec::new();
    for number in 0..300 {
        let status = if number % 10 == 0 {
            shop::Status::Pending
        } else {
            shop::Status::Approved
        };
        let name = format!("ร้าน{} {number}", words[number % words.len()]);
        shops.push(s.shop(&name, status, number % 3 != 0).await);
    }
    let description = "ทำสดใหม่ทุกวัน วัตถุดิบคัดอย่างดีจากในหมู่บ้าน สั่งล่วงหน้าได้ ".repeat(4);
    for chunk in 0..10 {
        let products = (0..1_000).map(|i| {
            let number = chunk * 1_000 + i;
            let name = format!("{} สูตร {number}", words[number % words.len()]);
            product::ActiveModel {
                description: Set(Some(description.clone())),
                updated_at: Set(testing::at() + TimeDelta::seconds(number.try_into().unwrap())),
                ..testing::new_product(shops[number % shops.len()], s.eggs, &name, Some(5))
            }
        });
        product::Entity::insert_many(products)
            .exec_without_returning(&s.db)
            .await
            .unwrap();
    }
    s.db.execute_unprepared("ANALYZE").await.unwrap();

    for keyword in ["ไข่", "ทุเรียน", "ขนม 99", "ร้านกาแฟ สูตร"]
    {
        let search = Search::parse(keyword).unwrap();
        let select = listing::select(Listing::Search(&search), now());
        let statement = select.build(DbBackend::Postgres);
        let explain = Statement {
            sql: format!("EXPLAIN (ANALYZE, FORMAT JSON) {}", statement.sql),
            ..statement
        };
        // Once to warm the cache, as a running server's is; then the fastest of a few
        s.db.query_one_raw(explain.clone()).await.unwrap();
        let mut fastest = f64::INFINITY;
        for _ in 0..5 {
            let row = s.db.query_one_raw(explain.clone()).await.unwrap().unwrap();
            let plan: serde_json::Value = row.try_get("", "QUERY PLAN").unwrap();
            let took = plan[0]["Execution Time"]
                .as_f64()
                .expect("an execution time");
            fastest = fastest.min(took);
        }
        assert!(fastest < 300.0, "{keyword}: {fastest} ms");
    }
    let found = s.found("กาแฟ สูตร 99").await;
    assert!(!found.is_empty());
}

/// FR-ADM-04: a suspended user can't order, but still browses.
#[tokio::test]
async fn suspended_users_still_browse() {
    let s = setup().await;
    s.list("ไข่ไก่", Some(5)).await;
    user::Entity::update_many()
        .col_expr(
            user::Column::Status,
            sea_orm::sea_query::Expr::value(user::Status::Suspended),
        )
        .filter(user::Column::Id.eq(s.buyer))
        .exec(&s.db)
        .await
        .unwrap();
    let list = s.tap(&format!("browse?category={}", s.eggs)).await;
    assert_eq!(names(&list), ["ไข่ไก่"]);
}
