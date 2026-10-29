//! The assistant's tools (Table 4.9-A), offered by the user's roles (FR-AI-01): read tools the
//! model calls as it likes, and proposal tools, which only ever offer a change for the user to
//! confirm with a tap (FR-AI-02, FR-AI-03). The two that start checkout and listing start the
//! same guided flows as the menus, which place an order or list a product only once the user
//! confirms their summary.
//!
//! Every call runs as the user, through the functions the menus use and their authorization
//! checks (NFR-SEC-02), whatever the model asked: a product buyers don't see, a shop's product
//! or an order not the user's, is not found. What the model is told back is compact, by
//! internal IDs, without phones, addresses, locations, PromptPay IDs or anyone's name but
//! shops' (FR-AI-08), and given as data (FR-AI-06); what the user sees are the system's cards
//! ([`Shown`]).
//!
//! Admins get only read tools (FR-AI-17): none of their own until "สถิติ" is built (#38), which
//! adds `get_admin_overview`; sellers' `get_sales_summary` comes with "ยอดขาย" (#39).

use sea_orm::{ActiveEnum, ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde_json::{Value, json};
use tracing::debug;

use super::shown::Shown;
use crate::ai::{Gateway, Schema, Tool, ToolCall, as_data};
use crate::catalog::browse;
use crate::catalog::browse::listing::{self, Listing};
use crate::catalog::browse::search::{KEYWORD_CHARS, Search};
use crate::catalog::categories;
use crate::catalog::products::{Availability, listing as drafting, manage};
use crate::catalog::shops::settings;
use crate::config::Config;
use crate::dispatcher::Turn;
use crate::dispatcher::flow;
use crate::line::Message;
use crate::notifications::templates::Browsed;
use crate::orders;
use crate::orders::cart::{self, AddToCart, Declined, NoRoom};
use crate::orders::checkout;
use crate::orders::lists::{self, ListedOrder};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{category, order, product, shop};
use crate::thai::Money;
use crate::users::access::{self, authorize};
use crate::users::help::{self, Topic};

/// The most of the shop's products `get_my_products` tells of, the newest first
const MY_PRODUCTS: u64 = 30;
/// The most keywords a search takes: the item and a few other words for it
const KEYWORDS: usize = 5;
/// The highest price a search takes, in baht: the most a product may cost (FR-PROD-03)
const MAX_PRICE: f64 = 100_000.0;

/// The tools of Table 4.9-A that #37 builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Name {
    SearchProducts,
    GetProduct,
    SearchShops,
    GetShop,
    ListCategories,
    GetHelp,
    GetCart,
    GetMyOrders,
    ProposeAddToCart,
    ProposeCheckout,
    ProposeCancelOrder,
    GetShopOrders,
    GetMyProducts,
    ProposeCreateProduct,
    ProposeUpdateProduct,
    ProposeSetShopOpen,
    ProposeOrderAction,
}

impl Name {
    const ALL: [Self; 17] = [
        Self::SearchProducts,
        Self::GetProduct,
        Self::SearchShops,
        Self::GetShop,
        Self::ListCategories,
        Self::GetHelp,
        Self::GetCart,
        Self::GetMyOrders,
        Self::ProposeAddToCart,
        Self::ProposeCheckout,
        Self::ProposeCancelOrder,
        Self::GetShopOrders,
        Self::GetMyProducts,
        Self::ProposeCreateProduct,
        Self::ProposeUpdateProduct,
        Self::ProposeSetShopOpen,
        Self::ProposeOrderAction,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::SearchProducts => "search_products",
            Self::GetProduct => "get_product",
            Self::SearchShops => "search_shops",
            Self::GetShop => "get_shop",
            Self::ListCategories => "list_categories",
            Self::GetHelp => "get_help",
            Self::GetCart => "get_cart",
            Self::GetMyOrders => "get_my_orders",
            Self::ProposeAddToCart => "propose_add_to_cart",
            Self::ProposeCheckout => "propose_checkout",
            Self::ProposeCancelOrder => "propose_cancel_order",
            Self::GetShopOrders => "get_shop_orders",
            Self::GetMyProducts => "get_my_products",
            Self::ProposeCreateProduct => "propose_create_product",
            Self::ProposeUpdateProduct => "propose_update_product",
            Self::ProposeSetShopOpen => "propose_set_shop_open",
            Self::ProposeOrderAction => "propose_order_action",
        }
    }

    fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == name)
    }

    /// Whether only sellers get it (Table 4.9-A); the others are everyone's, buyers all.
    fn sellers_only(self) -> bool {
        matches!(
            self,
            Self::GetShopOrders
                | Self::GetMyProducts
                | Self::ProposeCreateProduct
                | Self::ProposeUpdateProduct
                | Self::ProposeSetShopOpen
                | Self::ProposeOrderAction
        )
    }

    /// What the user must be allowed to do to use it at all (NFR-SEC-02); a tool acting on an
    /// order or a product checks it is theirs too.
    fn requires(self) -> Option<access::Action> {
        match self {
            Self::SearchProducts
            | Self::GetProduct
            | Self::SearchShops
            | Self::GetShop
            | Self::ListCategories => Some(access::Action::Browse),
            Self::GetHelp => Some(access::Action::Help),
            Self::GetCart | Self::ProposeAddToCart | Self::ProposeCheckout => {
                Some(access::Action::Buy)
            }
            Self::GetMyProducts
            | Self::ProposeCreateProduct
            | Self::ProposeUpdateProduct
            | Self::ProposeSetShopOpen => Some(access::Action::ManageShop),
            // Their own orders, which they always see (FR-ADM-04); each order is checked
            Self::GetMyOrders
            | Self::ProposeCancelOrder
            | Self::GetShopOrders
            | Self::ProposeOrderAction => None,
        }
    }
}

/// What the seller may propose to do about an order, by its name for the AI, with the actions
/// of the order's buttons that do it (Table 4.6-B). Rejecting and cancelling ask why after the
/// tap, as from the menus (FR-ORD-04).
const ORDER_ACTIONS: [(&str, &[&str]); 8] = [
    ("accept", &[orders::ACCEPT]),
    ("reject", &[orders::REJECT]),
    ("ready", &[orders::READY]),
    ("delivering", &[orders::DELIVERING]),
    ("complete", &[orders::COMPLETE]),
    ("cancel", &[orders::SELLER_CANCEL, orders::GRANT_CANCEL]),
    ("paid", &[orders::MARK_PAID]),
    ("refunded", &[orders::MARK_REFUNDED]),
];

/// What a buyer's `propose_cancel_order` offers: cancelling, or asking the shop to
const CANCELLING: [&str; 2] = [orders::CANCEL_ORDER, orders::REQUEST_CANCEL];

/// The tools offered to the user, and what they offer from.
pub struct Toolbox {
    tools: Vec<Tool>,
    /// The active categories, which searches and listings choose from
    categories: Vec<category::Model>,
}

impl Toolbox {
    /// The tools for the actor's roles (FR-AI-01): everyone's, and sellers' to a seller.
    pub async fn of(turn: &Turn<'_>) -> Result<Self, DbErr> {
        let roles = turn.actor.roles();
        let categories = categories::active(turn.db).await?;
        let topics: Vec<&str> = Topic::for_roles(roles).map(Topic::key).collect();
        let tools = Name::ALL
            .into_iter()
            .filter(|name| roles.seller || !name.sellers_only())
            .map(|name| tool(name, &categories, &topics))
            .collect();
        Ok(Self { tools, categories })
    }

    pub fn tools(&self) -> Vec<Tool> {
        self.tools.clone()
    }

    fn offers(&self, name: Name) -> bool {
        self.tools.iter().any(|tool| tool.name == name.as_str())
    }

    fn category_named(&self, name: &str) -> Option<&category::Model> {
        self.categories
            .iter()
            .find(|category| category.name == name)
    }

    fn category_of(&self, product: &product::Model) -> Option<&str> {
        self.categories
            .iter()
            .find(|category| category.id == product.category_id)
            .map(|category| category.name.as_str())
    }
}

/// Tool `name` as the model is offered it, choosing from `categories` and help `topics`.
fn tool(name: Name, categories: &[category::Model], topics: &[&str]) -> Tool {
    let id =
        |what: &str| Schema::string().describe(format!("the id of a {what}, as a tool gave it"));
    let order_number = || {
        Schema::string()
            .max_length(30)
            .describe("an order's number, such as IM-20261022-0007")
    };
    let (description, parameters) = match name {
        Name::SearchProducts => {
            let keyword = Schema::string().max_length(KEYWORD_CHARS);
            let keywords = Schema::array(keyword).max_items(KEYWORDS).describe(
                "what the user looks for, in its usual Thai name, then other words for it; each \
                 is matched on its own inside product, category and shop names",
            );
            let mut parameters = Schema::object().property("keywords", keywords);
            if !categories.is_empty() {
                let names = categories.iter().map(|category| category.name.clone());
                parameters = parameters.optional("category", Schema::string().one_of(names));
            }
            let price = Schema::number()
                .minimum(0)
                .describe("the highest price for one unit, in baht");
            let delivery = Schema::boolean().describe("only shops that deliver");
            (
                "Find products for sale in the market. The user sees what is found as cards.",
                parameters
                    .optional("maxPrice", price)
                    .optional("deliveryOnly", delivery),
            )
        }
        Name::GetProduct => (
            "Read a product's details, such as its description, to answer about it. The user sees \
             its card.",
            Schema::object().property("productId", id("product")),
        ),
        Name::SearchShops => (
            "Find the market's shops by their name or description, or list them all without \
             keywords. The user sees their cards.",
            Schema::object().optional("keywords", Schema::string().max_length(KEYWORD_CHARS)),
        ),
        Name::GetShop => (
            "Read a shop's details: whether it is open, its hours, pickup and delivery. The user \
             sees its card.",
            Schema::object().property("shopId", id("shop")),
        ),
        Name::ListCategories => (
            "The market's product categories. The user gets them as buttons.",
            Schema::object(),
        ),
        Name::GetHelp => (
            "The built-in help on a topic: the only source for answering how to use the market. \
             The user sees it.",
            Schema::object().property("topic", Schema::string().one_of(topics.iter().copied())),
        ),
        Name::GetCart => (
            "The user's cart, by shop. The user sees it.",
            Schema::object(),
        ),
        Name::GetMyOrders => (
            "The user's orders as a buyer, newest first: open ones, finished ones, or all. The \
             user sees their cards.",
            Schema::object().optional("status", Schema::string().one_of(["open", "done"])),
        ),
        Name::ProposeAddToCart => (
            "Propose putting a quantity of a product in the cart. Nothing changes: the user sees \
             the product's card with a button to confirm.",
            Schema::object()
                .property("productId", id("product"))
                .property("quantity", Schema::integer().minimum(1).maximum(99)),
        ),
        Name::ProposeCheckout => (
            "Start ordering what the cart holds of one shop. The user is asked how to receive \
             and pay, then confirms the order.",
            Schema::object().property("shopId", id("shop")),
        ),
        Name::ProposeCancelOrder => (
            "Propose cancelling one of the user's orders, or asking its shop to. Nothing changes: \
             the user sees the order with a button to confirm.",
            Schema::object().property("orderNo", order_number()),
        ),
        Name::GetShopOrders => (
            "The orders of the user's shop: new, active, unpaid or done. The user sees their \
             cards.",
            Schema::object().optional("status", Schema::string().one_of(lists::filter_keys())),
        ),
        Name::GetMyProducts => (
            "The products of the user's shop. The user sees them.",
            Schema::object(),
        ),
        Name::ProposeCreateProduct => (
            "Draft a new product for the user's shop, from only what the seller wrote. Nothing \
             is listed: the user sees a preview to correct and confirm.",
            drafting::draft_schema(categories),
        ),
        Name::ProposeUpdateProduct => (
            "Propose a new price or stock for one of the shop's products. Nothing changes: the \
             user sees what changes, with a button to confirm.",
            Schema::object()
                .property("productId", id("product"))
                .optional(
                    "price",
                    Schema::string().describe("baht for one unit, digits only"),
                )
                .optional(
                    "stock",
                    Schema::string().describe("how many are for sale now, digits only, or ทำตามสั่ง"),
                ),
        ),
        Name::ProposeSetShopOpen => (
            "Propose opening the user's shop, or closing it for now. Nothing changes: the user \
             sees a button to confirm.",
            Schema::object().property("isOpen", Schema::boolean()),
        ),
        Name::ProposeOrderAction => {
            let actions = ORDER_ACTIONS.iter().map(|&(action, _)| action);
            (
                "Propose a step for one of the shop's orders. Nothing changes: the user sees the \
                 order with a button to confirm; rejecting and cancelling then ask why.",
                Schema::object()
                    .property("orderNo", order_number())
                    .property("action", Schema::string().one_of(actions)),
            )
        }
    };
    Tool {
        name: name.as_str().to_owned(),
        description: description.to_owned(),
        parameters,
    }
}

/// Runs `call` as the actor, showing what it finds or proposes in `shown`: what the model is told
/// back, as data (FR-AI-06).
pub async fn run(
    turn: &Turn<'_>,
    config: &Config,
    ai: &Gateway,
    toolbox: &Toolbox,
    call: &ToolCall,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    // Never: the gateway checks the call names a tool offered (FR-AI-05)
    let Some(name) = Name::named(&call.name).filter(|&name| toolbox.offers(name)) else {
        return Ok(Value::String(as_data(&call.name, &failed("not_offered"))));
    };
    let told = match name.requires() {
        Some(action) if authorize(turn.db, &turn.actor, action).await?.is_err() => {
            failed("not_allowed")
        }
        _ => {
            let args = &call.arguments;
            match name {
                Name::SearchProducts => search_products(turn, toolbox, args, shown).await?,
                Name::GetProduct => get_product(turn, toolbox, args, shown).await?,
                Name::SearchShops => search_shops(turn, args, shown).await?,
                Name::GetShop => get_shop(turn, args, shown).await?,
                Name::ListCategories => list_categories(toolbox, shown),
                Name::GetHelp => get_help(turn, config, ai, args, shown),
                Name::GetCart => get_cart(turn, shown).await?,
                Name::GetMyOrders => get_my_orders(turn, args, shown).await?,
                Name::ProposeAddToCart => propose_add_to_cart(turn, toolbox, args, shown).await?,
                Name::ProposeCheckout => propose_checkout(turn, config, args, shown).await?,
                Name::ProposeCancelOrder => propose_cancel_order(turn, args, shown).await?,
                Name::GetShopOrders => get_shop_orders(turn, args, shown).await?,
                Name::GetMyProducts => get_my_products(turn, config, shown).await?,
                Name::ProposeCreateProduct => {
                    propose_create_product(turn, config, args, shown).await?
                }
                Name::ProposeUpdateProduct => {
                    propose_update_product(turn, config, args, shown).await?
                }
                Name::ProposeSetShopOpen => propose_set_shop_open(turn, args, shown),
                Name::ProposeOrderAction => propose_order_action(turn, args, shown).await?,
            }
        }
    };
    // Which tool, never what it was asked or found (NFR-PRIV-05)
    debug!(
        tool = name.as_str(),
        failed = told.get("error").is_some(),
        "assistant tool ran"
    );
    Ok(Value::String(as_data(name.as_str(), &told)))
}

async fn search_products(
    turn: &Turn<'_>,
    toolbox: &Toolbox,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let keywords = args["keywords"].as_array().map_or(&[][..], Vec::as_slice);
    let keywords = keywords.iter().filter_map(Value::as_str);
    let Ok(mut search) = Search::any_of(keywords) else {
        return Ok(failed("no_keywords"));
    };
    if let Some(category) = text(args, "category").and_then(|name| toolbox.category_named(name)) {
        search = search.in_category(category.id);
    }
    if let Some(price) = args["maxPrice"].as_f64().filter(|price| *price >= 0.0) {
        // In satang, rounded; a price past the most a product may cost is no limit
        let satang = (price.min(MAX_PRICE) * 100.0).round();
        #[allow(clippy::cast_possible_truncation, reason = "at most 10,000,000")]
        let satang = satang as i64;
        search = search.at_most(Money::from_satang(satang));
    }
    if args["deliveryOnly"].as_bool() == Some(true) {
        search = search.delivered();
    }
    let found = listing::page(turn.db, Listing::Search(&search), turn.now, 0).await?;
    if found.items.is_empty() {
        // Where else to look (FR-SRCH-08)
        let suggestions = browse::suggestions(turn, Browsed::Search(search.keyword())).await?;
        shown.choices(suggestions);
        return Ok(json!({ "found": [] }));
    }
    let products: Vec<Value> = found
        .items
        .iter()
        .map(|(product, shop)| product_json(toolbox, product, shop))
        .collect();
    for (product, shop) in found.items {
        shown.product(product, shop);
    }
    Ok(json!({ "found": products, "more": found.next.is_some() }))
}

async fn get_product(
    turn: &Turn<'_>,
    toolbox: &Toolbox,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let Some(id) = id(args, "productId") else {
        return Ok(failed("not_found"));
    };
    let Some((product, shop)) = listing::product(turn.db, id).await? else {
        return Ok(failed("not_found"));
    };
    let mut told = product_json(toolbox, &product, &shop);
    told["description"] = json!(product.description);
    shown.product(product, shop);
    Ok(told)
}

async fn search_shops(turn: &Turn<'_>, args: &Value, shown: &mut Shown) -> Result<Value, DbErr> {
    let found = match text(args, "keywords").filter(|keywords| !keywords.trim().is_empty()) {
        Some(keywords) => listing::find_shops(turn.db, keywords, 0).await?,
        None => listing::shops(turn.db, 0).await?,
    };
    let shops: Vec<Value> = found.items.iter().map(shop_json).collect();
    for shop in found.items {
        shown.shop(shop);
    }
    Ok(json!({ "found": shops, "more": found.next.is_some() }))
}

async fn get_shop(turn: &Turn<'_>, args: &Value, shown: &mut Shown) -> Result<Value, DbErr> {
    let Some(id) = id(args, "shopId") else {
        return Ok(failed("not_found"));
    };
    let Some(shop) = listing::shop(turn.db, id).await? else {
        return Ok(failed("not_found"));
    };
    let told = shop_json(&shop);
    shown.shop(shop);
    Ok(told)
}

fn list_categories(toolbox: &Toolbox, shown: &mut Shown) -> Value {
    let buttons = toolbox
        .categories
        .iter()
        .map(browse::category_button)
        .collect();
    shown.choices(buttons);
    let names: Vec<&str> = toolbox
        .categories
        .iter()
        .map(|category| category.name.as_str())
        .collect();
    json!({ "categories": names })
}

/// How-to questions are answered from this alone (FR-AI-14).
fn get_help(
    turn: &Turn<'_>,
    config: &Config,
    ai: &Gateway,
    args: &Value,
    shown: &mut Shown,
) -> Value {
    let topic = text(args, "topic").and_then(Topic::from_key);
    let Some((article, message)) = topic.and_then(|topic| help::topic(turn, config, ai, topic))
    else {
        return failed("not_found");
    };
    shown.messages([message]);
    json!({ "article": article })
}

async fn get_cart(turn: &Turn<'_>, shown: &mut Shown) -> Result<Value, DbErr> {
    let lines = cart::lines(turn.db, turn.actor.user.id).await?;
    let shops: Vec<Value> = lines
        .chunk_by(|a, b| a.shop.id == b.shop.id)
        .map(|lines| {
            let shop = &lines[0].shop;
            let lines: Vec<Value> = lines
                .iter()
                .map(|line| {
                    json!({
                        "productId": line.product.id.to_string(),
                        "name": line.product.name,
                        "quantity": line.item.quantity,
                        "unit": line.product.unit,
                        "total": line.total().to_string(),
                        "orderable": line.state.orderable(),
                    })
                })
                .collect();
            json!({ "shop": { "id": shop.id.to_string(), "name": shop.name }, "lines": lines })
        })
        .collect();
    shown.messages(cart::show(turn).await?);
    Ok(json!({ "shops": shops }))
}

async fn get_my_orders(turn: &Turn<'_>, args: &Value, shown: &mut Shown) -> Result<Value, DbErr> {
    let open = match text(args, "status") {
        Some("open") => Some(true),
        Some("done") => Some(false),
        _ => None,
    };
    let listed = lists::mine_for_assistant(turn, open).await?;
    let orders: Vec<Value> = listed
        .orders
        .iter()
        .map(|listed| order_json(turn, listed, true))
        .collect();
    remember_orders(shown, &listed.orders);
    shown.messages(listed.messages);
    Ok(json!({ "orders": orders }))
}

async fn propose_add_to_cart(
    turn: &Turn<'_>,
    toolbox: &Toolbox,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let Some(id) = id(args, "productId") else {
        return Ok(failed("not_found"));
    };
    let quantity = args["quantity"]
        .as_i64()
        .and_then(|quantity| i32::try_from(quantity).ok())
        .unwrap_or(0);
    let declined = match AddToCart::offer(turn, id, quantity).await? {
        Ok(offer) => {
            let told = product_json(toolbox, &offer.product, &offer.shop);
            shown.proposal(offer.product, offer.shop, offer.confirm);
            return Ok(json!({ "proposed": true, "quantity": quantity, "product": told }));
        }
        Err(declined) => declined,
    };
    // Its card shows why, from the database: how many are left, sold out or closed
    if !matches!(declined, Declined::NoRoom(NoRoom::NotFound))
        && let Some((product, shop)) = listing::product(turn.db, id).await?
    {
        shown.product(product, shop);
    }
    let mut told = failed(declined.code());
    if let Declined::TooMany(most) = declined {
        told["most"] = json!(most);
    }
    Ok(told)
}

async fn propose_checkout(
    turn: &Turn<'_>,
    config: &Config,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let Some(id) = id(args, "shopId") else {
        return Ok(failed("not_found"));
    };
    let lines = cart::lines(turn.db, turn.actor.user.id).await?;
    if !lines
        .iter()
        .any(|line| line.shop.id == id && line.state.orderable())
    {
        shown.messages(cart::show(turn).await?);
        return Ok(failed("nothing_to_order"));
    }
    let asked = checkout::begin(turn, config, id).await?;
    started(turn, asked, shown).await
}

async fn propose_cancel_order(
    turn: &Turn<'_>,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let number = text(args, "orderNo").unwrap_or_default();
    let Some(order) = lists::numbered(turn, number, ActorType::Buyer).await? else {
        return Ok(failed("not_found"));
    };
    if authorize(turn.db, &turn.actor, access::Action::ActAsBuyer(order.id))
        .await?
        .is_err()
    {
        return Ok(failed("not_allowed"));
    }
    shown.remember(json!({ "order": order.order_number }));
    let keep = |action: &str| CANCELLING.contains(&action);
    if let Some(card) = lists::proposal_card(turn, &order, ActorType::Buyer, keep).await? {
        shown.messages([card]);
        return Ok(json!({ "proposed": true, "status": status(&order) }));
    }
    shown.messages([lists::full_card(turn, &order, ActorType::Buyer).await?]);
    Ok(json!({ "error": "not_now", "status": status(&order) }))
}

async fn get_shop_orders(turn: &Turn<'_>, args: &Value, shown: &mut Shown) -> Result<Value, DbErr> {
    let listed = lists::shop_for_assistant(turn, text(args, "status")).await?;
    let orders: Vec<Value> = listed
        .orders
        .iter()
        .map(|listed| order_json(turn, listed, false))
        .collect();
    remember_orders(shown, &listed.orders);
    shown.messages(listed.messages);
    Ok(json!({ "orders": orders }))
}

async fn get_my_products(
    turn: &Turn<'_>,
    config: &Config,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let Some(shop) = &turn.actor.shop else {
        return Ok(failed("not_allowed"));
    };
    let mine = product::Entity::find()
        .filter(product::Column::ShopId.eq(shop.id))
        .filter(product::Column::Status.ne(product::Status::Deleted))
        .order_by_desc(product::Column::Id)
        .limit(MY_PRODUCTS)
        .all(turn.db)
        .await?;
    let told: Vec<Value> = mine
        .iter()
        .map(|product| {
            json!({
                "id": product.id.to_string(),
                "name": product.name,
                "unit": product.unit,
                "price": product.price.to_string(),
                "stock": product.stock,
                "status": product.status.to_value().to_lowercase(),
            })
        })
        .collect();
    for product in &mine {
        shown.remember(json!({ "product": product.id.to_string(), "name": product.name }));
    }
    shown.messages([manage::list(turn.db, turn.templates, config, shop.id, 0).await?]);
    Ok(json!({ "products": told }))
}

/// The draft is taken part by part as if typed at its steps, so each is checked as "เพิ่มสินค้า"
/// checks it; it invents nothing (FR-AI-12).
async fn propose_create_product(
    turn: &Turn<'_>,
    config: &Config,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let Some(asked) = drafting::propose_new(turn, config, args).await? else {
        return Ok(failed("not_allowed"));
    };
    started(turn, asked, shown).await
}

async fn propose_update_product(
    turn: &Turn<'_>,
    config: &Config,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let Some(id) = id(args, "productId") else {
        return Ok(failed("not_found"));
    };
    // Whichever product the AI chose, the seller must be allowed to change it (FR-AI-06)
    if authorize(turn.db, &turn.actor, access::Action::ManageProduct(id))
        .await?
        .is_err()
    {
        return Ok(failed("not_found"));
    }
    let update = json!({
        "product": id.to_string(),
        "price": args.get("price"),
        "stock": args.get("stock"),
    });
    let Some(proposed) = drafting::propose_update(turn, config, &update).await? else {
        return Ok(failed("not_allowed"));
    };
    shown.messages(proposed);
    Ok(json!({ "shown": "the change to confirm, or why it can't be made" }))
}

fn propose_set_shop_open(turn: &Turn<'_>, args: &Value, shown: &mut Shown) -> Value {
    let (Some(shop), Some(open)) = (&turn.actor.shop, args["isOpen"].as_bool()) else {
        return failed("not_allowed");
    };
    if shop.is_open == open {
        return json!({ "error": "already", "open": open });
    }
    let switch = settings::switch(shop.is_open);
    let confirm = turn.templates.confirm_shop_open(&shop.name, open, switch);
    shown.messages([confirm]);
    json!({ "proposed": true })
}

async fn propose_order_action(
    turn: &Turn<'_>,
    args: &Value,
    shown: &mut Shown,
) -> Result<Value, DbErr> {
    let number = text(args, "orderNo").unwrap_or_default();
    let Some(order) = lists::numbered(turn, number, ActorType::Seller).await? else {
        return Ok(failed("not_found"));
    };
    if authorize(turn.db, &turn.actor, access::Action::HandleOrder(order.id))
        .await?
        .is_err()
    {
        return Ok(failed("not_allowed"));
    }
    shown.remember(json!({ "order": order.order_number }));
    let wanted = text(args, "action");
    let acts = ORDER_ACTIONS
        .iter()
        .find(|&&(action, _)| Some(action) == wanted)
        .map_or(&[][..], |&(_, acts)| acts);
    let keep = |action: &str| acts.contains(&action);
    if let Some(card) = lists::proposal_card(turn, &order, ActorType::Seller, keep).await? {
        shown.messages([card]);
        return Ok(json!({ "proposed": true, "status": status(&order) }));
    }
    shown.messages([lists::full_card(turn, &order, ActorType::Seller).await?]);
    Ok(json!({ "error": "not_now", "status": status(&order) }))
}

/// What a tool that starts a guided flow told, its messages `asked` shown last if it did; if it
/// was refused instead, why, as shown.
async fn started(turn: &Turn<'_>, asked: Vec<Message>, shown: &mut Shown) -> Result<Value, DbErr> {
    if flow::current(turn).await?.is_some() {
        shown.flow(asked);
        return Ok(json!({ "started": true }));
    }
    shown.messages(asked);
    Ok(failed("not_now"))
}

fn remember_orders(shown: &mut Shown, orders: &[ListedOrder]) {
    for listed in orders {
        shown.remember(json!({ "order": listed.order.order_number }));
    }
}

/// `product` of `shop`, as the model is told of it: no more than its card shows.
fn product_json(toolbox: &Toolbox, product: &product::Model, shop: &shop::Model) -> Value {
    let availability = match Availability::of(product, shop) {
        Availability::Left(left) => json!({ "left": left }),
        Availability::Orderable => json!("available"),
        Availability::SoldOut => json!("sold_out"),
        Availability::ShopClosed => json!("shop_closed"),
    };
    json!({
        "id": product.id.to_string(),
        "name": product.name,
        "unit": product.unit,
        "price": product.price.to_string(),
        "availability": availability,
        "category": toolbox.category_of(product),
        "shop": { "id": shop.id.to_string(), "name": shop.name, "delivers": shop.offers_delivery },
    })
}

/// `shop`, as buyers see it on its card, without its phone, PromptPay ID or pickup address
/// (FR-AI-08).
fn shop_json(shop: &shop::Model) -> Value {
    json!({
        "id": shop.id.to_string(),
        "name": shop.name,
        "description": shop.description,
        "open": shop.is_open,
        "openingHours": shop.opening_hours,
        "pickup": shop.offers_pickup,
        "delivery": shop.offers_delivery,
        "deliveryFee": shop.offers_delivery.then(|| shop.delivery_fee.to_string()),
        "minimumOrder": shop.minimum_order.map(|minimum| minimum.to_string()),
    })
}

/// An order as the model is told of it: its number, status, lines and amounts, its shop's name
/// `as_buyer`; never its address, note, location or buyer (FR-AI-08).
fn order_json(turn: &Turn<'_>, listed: &ListedOrder, as_buyer: bool) -> Value {
    let order = &listed.order;
    let items: Vec<Value> = listed
        .items
        .iter()
        .map(|item| json!({ "name": item.name, "quantity": item.quantity, "unit": item.unit }))
        .collect();
    json!({
        "orderNo": order.order_number,
        "status": status(order),
        "paymentStatus": order.payment_status.to_value().to_lowercase(),
        "payment": order.payment_method.to_value().to_lowercase(),
        "fulfillment": order.fulfillment.to_value().to_lowercase(),
        "shop": as_buyer.then_some(listed.shop.as_str()),
        "items": items,
        "total": order.total.to_string(),
        "placedAt": turn.templates.date_time(order.created_at),
    })
}

fn status(order: &order::Model) -> String {
    order.status.to_value().to_lowercase()
}

/// What a tool tells the model when it can't do what was asked: why, by name.
fn failed(code: &str) -> Value {
    json!({ "error": code })
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// An ID the model gave as `key`: digits, as the tools give them, or a number.
fn id(args: &Value, key: &str) -> Option<i64> {
    match args.get(key)? {
        Value::String(id) => id.trim().parse().ok(),
        Value::Number(id) => id.as_i64(),
        _ => None,
    }
}
