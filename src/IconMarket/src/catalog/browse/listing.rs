//! What buyers browse, a page at a time (FR-SRCH-02, FR-SRCH-03): the products of a category or
//! a shop, "ของใหม่วันนี้" (FR-SRCH-09) and the results of a search (FR-SRCH-04), and the shops
//! of "ร้านค้าทั้งหมด" (FR-SRCH-06). The AI assistant's searches (#37) and the village group
//! digest (#48) list from here too, so that every list shows buyers the same products.
//!
//! Buyers see only `ACTIVE` products of `APPROVED` shops: never a hidden, held, removed or
//! deleted product, nor one of a shop that is pending, held, suspended, rejected or closed
//! (FR-PROD-07, FR-SHOP-10). A closed-for-now shop is still `APPROVED`: its products stay
//! listed, after the open shops' (FR-SHOP-05). Nor do they see the shop of a suspended user,
//! who can't sell (FR-ADM-04), until the suspension is lifted.
//!
//! Lists are paged in the database, a page and one more row at a time, so a long list never
//! loads whole. A search scans the products, their descriptions and their shops' and categories'
//! names: at the target volume of 10,000 products it stays well within NFR-PERF-05's 300 ms of
//! database time, as a test run alone checks, so it needs no index the models can't declare
//! (MIG-01).

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::extension::postgres::PgExpr;
use sea_orm::sea_query::{Expr, ExprTrait, Order};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait, SelectTwoRequired,
};

use super::search::{Search, escaped};
use crate::notifications::templates::{PRODUCTS_PER_PAGE, SHOPS_PER_PAGE};
use crate::persistence::entity::{product, shop, user};
use crate::persistence::paging::{Page, cut, paged};

/// How long a product stays in "ของใหม่วันนี้" once listed or restocked (FR-SRCH-09)
pub const NEW_FOR: TimeDelta = TimeDelta::hours(24);

/// A list of products buyers browse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing<'a> {
    /// Those of the category with this ID (FR-SRCH-01)
    Category(i64),
    /// Those of the shop with this ID, from its card's "ดูสินค้า" (FR-SHOP-09)
    Shop(i64),
    /// "ของใหม่วันนี้": those listed or restocked within [`NEW_FOR`] (FR-SRCH-09)
    NewToday,
    /// Those a search finds (FR-SRCH-04)
    Search(&'a Search),
}

/// A product as buyers see it, with its shop.
pub type Listed = (product::Model, shop::Model);

/// The products of `listing` at `now` that buyers see, with their shops, in the order lists
/// show them (FR-SRCH-02): open shops' first, then those in stock, then the most recently
/// changed. The product's ID comes last, so pages never overlap.
pub fn select(
    listing: Listing<'_>,
    now: DateTimeUtc,
) -> SelectTwoRequired<product::Entity, shop::Entity> {
    let in_stock = Expr::col((product::Entity, product::Column::Stock))
        .is_null()
        .or(Expr::col((product::Entity, product::Column::Stock)).gt(0));
    let select = visible();
    let select = match listing {
        Listing::Category(id) => select.filter(product::Column::CategoryId.eq(id)),
        Listing::Shop(id) => select.filter(product::Column::ShopId.eq(id)),
        Listing::NewToday => {
            let since = now - NEW_FOR;
            select.filter(
                Condition::any()
                    .add(product::Column::CreatedAt.gte(since))
                    .add(product::Column::RestockedAt.gte(since)),
            )
        }
        Listing::Search(search) => select
            .join(JoinType::InnerJoin, product::Relation::Category.def())
            .filter(search.condition()),
    };
    select
        .order_by_desc(shop::Column::IsOpen)
        .order_by(in_stock, Order::Desc)
        .order_by_desc(product::Column::UpdatedAt)
        .order_by_desc(product::Column::Id)
}

/// Page `page` (from 0) of `listing` at `now`, [`PRODUCTS_PER_PAGE`] products a page. A page
/// past the last is empty.
pub async fn page(
    db: &impl ConnectionTrait,
    listing: Listing<'_>,
    now: DateTimeUtc,
    page: usize,
) -> Result<Page<Listed>, DbErr> {
    let rows = paged(select(listing, now), page, PRODUCTS_PER_PAGE)
        .all(db)
        .await?;
    Ok(cut(rows, page, PRODUCTS_PER_PAGE))
}

/// Product `id` and its shop, if buyers see it: its details, from a tap on its card
/// (FR-SRCH-07).
pub async fn product(db: &impl ConnectionTrait, id: i64) -> Result<Option<Listed>, DbErr> {
    visible().filter(product::Column::Id.eq(id)).one(db).await
}

/// Shop `id`, if buyers see it: `APPROVED`, open for now or not (FR-SHOP-10), and its owner not
/// suspended (FR-ADM-04).
pub async fn shop(db: &impl ConnectionTrait, id: i64) -> Result<Option<shop::Model>, DbErr> {
    selling(shop::Entity::find_by_id(id)).one(db).await
}

/// Page `page` (from 0) of "ร้านค้าทั้งหมด" (FR-SRCH-06): the shops buyers see, open ones
/// first, then by name, [`SHOPS_PER_PAGE`] a page. A page past the last is empty.
pub async fn shops(db: &impl ConnectionTrait, page: usize) -> Result<Page<shop::Model>, DbErr> {
    let select = selling(shop::Entity::find())
        .order_by_desc(shop::Column::IsOpen)
        .order_by_asc(shop::Column::Name)
        .order_by_asc(shop::Column::Id);
    let rows = paged(select, page, SHOPS_PER_PAGE).all(db).await?;
    Ok(cut(rows, page, SHOPS_PER_PAGE))
}

/// Page `page` (from 0) of the shops buyers see whose name or description holds each word of
/// `keyword`, whatever the case, in the order of "ร้านค้าทั้งหมด": the AI assistant's
/// `search_shops` (FR-AI-02). `keyword` is matched as written, never as a pattern.
pub async fn find_shops(
    db: &impl ConnectionTrait,
    keyword: &str,
    page: usize,
) -> Result<Page<shop::Model>, DbErr> {
    let mut words: Vec<&str> = Vec::new();
    for word in keyword.split_whitespace() {
        if !words.contains(&word) {
            words.push(word);
        }
    }
    let condition = words.into_iter().fold(Condition::all(), |all, word| {
        let pattern = || format!("%{}%", escaped(word));
        let any = Condition::any()
            .add(Expr::col((shop::Entity, shop::Column::Name)).ilike(pattern()))
            .add(Expr::col((shop::Entity, shop::Column::Description)).ilike(pattern()));
        all.add(any)
    });
    let select = selling(shop::Entity::find())
        .filter(condition)
        .order_by_desc(shop::Column::IsOpen)
        .order_by_asc(shop::Column::Name)
        .order_by_asc(shop::Column::Id);
    let rows = paged(select, page, SHOPS_PER_PAGE).all(db).await?;
    Ok(cut(rows, page, SHOPS_PER_PAGE))
}

/// The products buyers see, with their shops: `ACTIVE` ones of the shops they see.
fn visible() -> SelectTwoRequired<product::Entity, shop::Entity> {
    let products = product::Entity::find()
        .join(JoinType::InnerJoin, product::Relation::Shop.def())
        .filter(product::Column::Status.eq(product::Status::Active));
    selling(products).select_two_required(shop::Entity)
}

/// `select`, of shops or joined to them, limited to the shops buyers see: `APPROVED`, of an
/// owner who isn't suspended.
fn selling<S: QuerySelect + QueryFilter>(select: S) -> S {
    select
        .join(JoinType::InnerJoin, shop::Relation::Owner.def())
        .filter(shop::Column::Status.eq(shop::Status::Approved))
        .filter(user::Column::Status.eq(user::Status::Active))
}
