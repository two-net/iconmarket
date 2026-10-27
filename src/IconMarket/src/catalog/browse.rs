//! "ซื้อของ" (FR-SRCH-01): "ของใหม่วันนี้" (FR-SRCH-09), "ร้านค้าทั้งหมด" (FR-SRCH-06) and
//! the active categories, in the order admins set (FR-ADM-05), as Quick Replies. A category
//! lists its products, and so does "ดูสินค้า" on a shop's card (FR-SHOP-09); "ค้นหา" is
//! [`search`]. What each list holds, and in what order, is [`listing`].
//!
//! A list is a carousel of product cards, ten a page, with "ดูเพิ่มเติม" for the next
//! (FR-SRCH-03). A card shows the product's first photo, or a placeholder, its name, price per
//! unit and shop, and whether it can be ordered: how many are left, made to order, sold out or
//! its shop closed (FR-SRCH-07). Its "ดูร้าน" shows the shop's card; its "ใส่ตะกร้า", only
//! while it can be ordered (FR-PROD-06), puts it in the [cart](orders::cart); a tap anywhere
//! else shows its details: every photo, its description and its shop. When a list has nothing
//! to show, the reply says so and suggests where else to look (FR-SRCH-08).
//!
//! Every button carries only IDs and page numbers, and each tap finds what it names afresh: a
//! product or shop that buyers no longer see is not found (FR-UI-13). Anyone may browse,
//! suspended users too (FR-ADM-04).

pub mod listing;
pub mod search;
#[cfg(test)]
mod tests;

use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};
use url::Url;

use self::listing::{Listed, Listing, Page};
use super::images::{self, Owner};
use super::products::Availability;
use super::{categories, shops};
use crate::config::Config;
use crate::dispatcher::Turn;
use crate::dispatcher::menu::{self, Item};
use crate::dispatcher::postback::Postback;
use crate::line::{Action, Message, QuickReply, limits};
use crate::notifications::templates::{
    self, Browsed, ProductCard, ProductDetails, ShopFront, labels,
};
use crate::orders;
use crate::persistence::entity::{category, product, shop};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};
use crate::users::report::{self, Target};

/// The action of a category's Quick Reply; its `category` is the category's ID
pub const CATEGORY: &str = "browse";
const CATEGORY_ID: &str = "category";
/// What the actions of browsing's other buttons start with
pub const PREFIX: &str = "browse.";
/// The action of "ดูสินค้า" on a shop's card (FR-SHOP-09); its `shop` is the shop's ID
pub const SHOP: &str = "browse.shop";
pub const SHOP_ID: &str = "shop";
/// The action of "ของใหม่วันนี้"
const NEW_TODAY: &str = "browse.new";
/// The action of "ร้านค้าทั้งหมด"
const SHOPS: &str = "browse.shops";
/// The action of a tap on a product's card; its `product` is the product's ID
const PRODUCT: &str = "browse.product";
const PRODUCT_ID: &str = "product";
/// The action of "ดูร้าน" on a product's card; its `shop` is the shop's ID
const FRONT: &str = "browse.front";
/// The page a list's "ดูเพิ่มเติม" shows, on its postback
const PAGE: &str = "page";
/// Categories on a page of Quick Replies, after "ของใหม่วันนี้" and "ร้านค้าทั้งหมด", leaving
/// room for "ดูเพิ่มเติม"
const CATEGORIES_PER_PAGE: usize = limits::QUICK_REPLY_ITEMS - 3;

/// A tap on "ซื้อของ", or on a button of browsing.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Browse).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let page = postback
        .int(PAGE)
        .and_then(|page| usize::try_from(page).ok())
        .unwrap_or(0);
    let id = |key| postback.int(key);
    match (
        postback.action(),
        id(CATEGORY_ID),
        id(SHOP_ID),
        id(PRODUCT_ID),
    ) {
        (CATEGORY, Some(id), _, _) => category(turn, config, id, page).await,
        (SHOP, _, Some(id), _) => shop(turn, config, id, page).await,
        (NEW_TODAY, ..) => {
            let (listing, more) = (Listing::NewToday, Postback::new(NEW_TODAY));
            listed(turn, config, listing, Browsed::NewToday, page, more).await
        }
        (SHOPS, ..) => all_shops(turn, config, page).await,
        (PRODUCT, _, _, Some(id)) => details(turn, config, id).await,
        (FRONT, _, Some(id), _) => front(turn, config, id).await,
        (search::MORE, ..) => search::more(turn, config, postback).await,
        (action, ..) if action == Item::Buy.action() => choose(turn, page).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// "ซื้อของ" at page `page` of its categories.
async fn choose(turn: &Turn<'_>, page: usize) -> Result<Vec<Message>, DbErr> {
    let active = categories::active(turn.db).await?;
    if active.is_empty() {
        let none = turn.templates.nothing_to_browse();
        return Ok(vec![none, menu::main_menu(&turn.actor)]);
    }
    // A page past the last, from a button sent before categories were deactivated, starts over
    let pages = active.len().div_ceil(CATEGORIES_PER_PAGE);
    let page = if page < pages { page } else { 0 };
    let more = |next| {
        let more = Item::Buy.postback().with_int(PAGE, next as i64);
        more.to_string()
    };
    let mut choices = vec![new_today(), all_shops_button()];
    choices.extend(templates::quick_reply_page(
        &active,
        page,
        CATEGORIES_PER_PAGE,
        category_button,
        more,
    ));
    Ok(vec![turn.templates.choose_category(choices)])
}

/// Page `page` of the products of category `id`, while it is active.
async fn category(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    page: usize,
) -> Result<Vec<Message>, DbErr> {
    let Some(category) = category::Entity::find_by_id(id)
        .filter(category::Column::IsActive.eq(true))
        .one(turn.db)
        .await?
    else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let more = Postback::new(CATEGORY).with_int(CATEGORY_ID, id);
    let browsed = Browsed::Category(&category.name);
    listed(turn, config, Listing::Category(id), browsed, page, more).await
}

/// Page `page` of the products of shop `id`, while buyers see it: its card's "ดูสินค้า".
async fn shop(
    turn: &Turn<'_>,
    config: &Config,
    id: i64,
    page: usize,
) -> Result<Vec<Message>, DbErr> {
    let Some(shop) = listing::shop(turn.db, id).await? else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let more = Postback::new(SHOP).with_int(SHOP_ID, id);
    let browsed = Browsed::Shop(&shop.name);
    listed(turn, config, Listing::Shop(id), browsed, page, more).await
}

/// Page `page` of `listing`, what was `browsed`. Its "ดูเพิ่มเติม" sends `more` with the next
/// page's number.
async fn listed(
    turn: &Turn<'_>,
    config: &Config,
    listing: Listing<'_>,
    browsed: Browsed<'_>,
    page: usize,
    more: Postback,
) -> Result<Vec<Message>, DbErr> {
    let found = page_of(turn, listing, page).await?;
    if found.items.is_empty() {
        let suggestions = suggestions(turn, browsed).await?;
        return Ok(vec![turn.templates.nothing_found(browsed, suggestions)]);
    }
    let more = found.next.map(|next| {
        let next = i64::try_from(next).expect("a few pages");
        more.with_int(PAGE, next)
    });
    Ok(vec![
        products(turn, config, browsed, &found.items, more).await?,
    ])
}

/// Page `page` of `listing`, or its first if there is no such page any more, as products went
/// since its button was sent.
async fn page_of(
    turn: &Turn<'_>,
    listing: Listing<'_>,
    page: usize,
) -> Result<Page<Listed>, DbErr> {
    let found = listing::page(turn.db, listing, turn.now, page).await?;
    if found.items.is_empty() && page > 0 {
        return listing::page(turn.db, listing, turn.now, 0).await;
    }
    Ok(found)
}

/// A page of a product list of what was `browsed`: a card for each of `listed`, and
/// "ดูเพิ่มเติม" sending `more` if more follow.
async fn products(
    turn: &Turn<'_>,
    config: &Config,
    browsed: Browsed<'_>,
    listed: &[Listed],
    more: Option<Postback>,
) -> Result<Message, DbErr> {
    let ids: Vec<i64> = listed.iter().map(|(product, _)| product.id).collect();
    let photos = images::of_products(turn.db, &ids).await?;
    let base = &config.app.base_url;
    let placeholder = images::placeholder_url(base);
    let urls: Vec<Url> = listed
        .iter()
        .map(|(product, _)| {
            let first = photos.get(&product.id).and_then(|photos| photos.first());
            first.map_or_else(
                || placeholder.clone(),
                |&id| images::product_image_url(base, id),
            )
        })
        .collect();
    let cards: Vec<ProductCard<'_>> = listed
        .iter()
        .zip(&urls)
        .map(|((product, shop), photo)| card(product, shop, photo))
        .collect();
    let more = more.map(|more| more.to_string());
    Ok(turn
        .templates
        .product_list(browsed, &cards, more, carry_on()))
}

/// What `product` of `shop`'s card shows, with its first photo, or the placeholder, at `photo`.
fn card<'a>(product: &'a product::Model, shop: &'a shop::Model, photo: &'a Url) -> ProductCard<'a> {
    let availability = Availability::of(product, shop);
    let details = Postback::new(PRODUCT).with_int(PRODUCT_ID, product.id);
    ProductCard {
        name: &product.name,
        price: product.price,
        unit: &product.unit,
        shop: &shop.name,
        availability,
        photo,
        add_to_cart: availability.orderable().then(|| add_to_cart(product.id)),
        view_shop: view_shop(shop.id),
        details: details.button(Label::fit(product.name.as_str())),
    }
}

/// Product `id`'s details, while buyers see it: a tap on its card (FR-SRCH-07).
async fn details(turn: &Turn<'_>, config: &Config, id: i64) -> Result<Vec<Message>, DbErr> {
    let Some((product, shop)) = listing::product(turn.db, id).await? else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let base = &config.app.base_url;
    let photos = images::all(turn.db, Owner::Product(product.id)).await?;
    let photos: Vec<Url> = if photos.is_empty() {
        vec![images::placeholder_url(base)]
    } else {
        photos
            .into_iter()
            .map(|id| images::product_image_url(base, id))
            .collect()
    };
    let availability = Availability::of(&product, &shop);
    let mut buttons: Vec<Action> = availability
        .orderable()
        .then(|| add_to_cart(product.id))
        .into_iter()
        .collect();
    buttons.push(view_shop(shop.id));
    buttons.push(report::button(Target::Product(product.id)));
    let details = ProductDetails {
        name: &product.name,
        price: product.price,
        unit: &product.unit,
        description: product.description.as_deref(),
        shop: &shop.name,
        availability,
        photos: &photos,
        buttons,
    };
    Ok(vec![turn.templates.product_details(&details, carry_on())])
}

/// Shop `id`'s card, while buyers see it: "ดูร้าน" on a product's card (FR-SHOP-09).
async fn front(turn: &Turn<'_>, config: &Config, id: i64) -> Result<Vec<Message>, DbErr> {
    let Some(shop) = listing::shop(turn.db, id).await? else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    let photo = images::first(turn.db, Owner::Shop(shop.id))
        .await?
        .map(|id| images::product_image_url(&config.app.base_url, id));
    let front = shops::front(&shop, photo.as_ref());
    Ok(vec![
        turn.templates
            .shop_front(&front)
            .with_quick_reply(QuickReply::new(carry_on())),
    ])
}

/// Page `page` of "ร้านค้าทั้งหมด" (FR-SRCH-06).
async fn all_shops(turn: &Turn<'_>, config: &Config, page: usize) -> Result<Vec<Message>, DbErr> {
    let mut found = listing::shops(turn.db, page).await?;
    if found.items.is_empty() && page > 0 {
        found = listing::shops(turn.db, 0).await?;
    }
    if found.items.is_empty() {
        let suggestions = suggestions(turn, Browsed::Shops).await?;
        return Ok(vec![
            turn.templates.nothing_found(Browsed::Shops, suggestions),
        ]);
    }
    let ids: Vec<i64> = found.items.iter().map(|shop| shop.id).collect();
    let photos = images::of_shops(turn.db, &ids).await?;
    let base = &config.app.base_url;
    let urls: Vec<Option<Url>> = found
        .items
        .iter()
        .map(|shop| {
            let first = photos.get(&shop.id).and_then(|photos| photos.first());
            first.map(|&id| images::product_image_url(base, id))
        })
        .collect();
    let fronts: Vec<ShopFront<'_>> = found
        .items
        .iter()
        .zip(&urls)
        .map(|(shop, photo)| shops::front(shop, photo.as_ref()))
        .collect();
    let more = found.next.map(|next| {
        let next = i64::try_from(next).expect("a few pages");
        Postback::new(SHOPS).with_int(PAGE, next).to_string()
    });
    Ok(vec![turn.templates.shop_list(&fronts, more, carry_on())])
}

/// Where else to look when what was `browsed` has nothing (FR-SRCH-08): another search, the
/// other lists of "ซื้อของ" and the active categories, as many as fit, then the menu.
async fn suggestions(turn: &Turn<'_>, browsed: Browsed<'_>) -> Result<Vec<Action>, DbErr> {
    let mut suggestions = vec![Item::Search.postback().button(labels::SEARCH_AGAIN.clone())];
    if browsed != Browsed::NewToday {
        suggestions.push(new_today());
    }
    if browsed != Browsed::Shops {
        suggestions.push(all_shops_button());
    }
    let room = limits::QUICK_REPLY_ITEMS - suggestions.len() - 1;
    let categories = categories::active(turn.db).await?;
    suggestions.extend(
        categories
            .iter()
            .filter(|category| browsed != Browsed::Category(&category.name))
            .take(room)
            .map(category_button),
    );
    suggestions.push(menu_button());
    Ok(suggestions)
}

/// Where to go on from a list: "ซื้อของ", "ค้นหา" and the menu.
fn carry_on() -> Vec<Action> {
    vec![
        Item::Buy.postback().button(labels::BUY.clone()),
        Item::Search.postback().button(labels::SEARCH.clone()),
        menu_button(),
    ]
}

fn menu_button() -> Action {
    Action::message(labels::MENU.clone(), labels::MENU.as_str())
}

/// A category's Quick Reply, which lists its products.
fn category_button(category: &category::Model) -> Action {
    let browse = Postback::new(CATEGORY).with_int(CATEGORY_ID, category.id);
    browse.button(Label::fit(category.name.as_str()))
}

fn new_today() -> Action {
    Postback::new(NEW_TODAY).button(labels::NEW_TODAY.clone())
}

fn all_shops_button() -> Action {
    Postback::new(SHOPS).button(labels::ALL_SHOPS.clone())
}

/// "ดูร้าน" on a product's card: shop `shop_id`'s card.
fn view_shop(shop_id: i64) -> Action {
    let front = Postback::new(FRONT).with_int(SHOP_ID, shop_id);
    front.button(labels::VIEW_SHOP.clone())
}

/// "ใส่ตะกร้า" on product `product_id`'s card.
fn add_to_cart(product_id: i64) -> Action {
    let add = Postback::new(orders::ADD_TO_CART).with_int(orders::PRODUCT_ID, product_id);
    add.button(labels::ADD_TO_CART.clone())
}
