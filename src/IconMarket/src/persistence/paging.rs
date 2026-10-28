//! Lists paged in the database, a page and one more row at a time, so a long list never loads
//! whole (FR-LINE-11): the products and shops buyers browse, and the orders of "คำสั่งซื้อของฉัน"
//! and "ออเดอร์ร้าน" (FR-ORD-10). The query's order must end with a unique column, so pages never
//! overlap.

use sea_orm::QuerySelect;

/// A page of a list: its items, in order, and the number of the next page if there is one.
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<usize>,
}

/// `select` limited to page `page` of `per_page` rows, and one more, which says whether a next
/// page follows.
pub fn paged<S: QuerySelect>(select: S, page: usize, per_page: usize) -> S {
    let offset = page.saturating_mul(per_page);
    select
        .offset(u64::try_from(offset).unwrap_or(u64::MAX))
        .limit(u64::try_from(per_page + 1).expect("a few rows"))
}

/// Page `page` of `rows`, as [`paged`] fetched them.
pub fn cut<T>(mut rows: Vec<T>, page: usize, per_page: usize) -> Page<T> {
    let next = (rows.len() > per_page).then_some(page + 1);
    rows.truncate(per_page);
    Page { items: rows, next }
}
