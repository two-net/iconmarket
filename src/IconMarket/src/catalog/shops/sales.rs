//! "ยอดขาย" (FR-SHOP-11): how the seller's shop is selling. For the orders placed today and in
//! the last 7 and 30 days, as [`periods`] counts them: how many have each status, the value of
//! those now `COMPLETED`, those of them still unpaid (FR-PAY-07), and the [`TOP`] products
//! that sold the most in them by value, since units differ from product to product. Only the
//! shop's own orders count (NFR-SEC-02).
//!
//! A product is named as its latest line in the period has it: the name it was sold under
//! (DR-03), even once renamed or deleted. Sellers read the same figures through the assistant's
//! `get_sales_summary`, which shows this card too (FR-AI-02).

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use chrono_tz::Tz;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::Order;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait,
};

use crate::config::Config;
use crate::dispatcher::Turn;
use crate::dispatcher::menu::Item;
use crate::line::Message;
use crate::notifications::templates::{Templates, labels};
use crate::orders::lists;
use crate::orders::periods::{self, Orders, PERIODS, since};
use crate::payments::UNPAID;
use crate::persistence::entity::{order, order_item, shop};
use crate::thai::Money;
use crate::users::access::{self, Denied, authorize};

/// The most products a period ranks
pub const TOP: u64 = 5;

/// What "ยอดขาย" shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sales {
    /// One for each of [`PERIODS`]
    pub periods: [Period; 3],
}

/// The shop's orders placed in a period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Period {
    pub orders: Orders,
    /// Of those now `COMPLETED`, the ones not yet paid (FR-PAY-07)
    pub unpaid: u64,
    /// Their total
    pub unpaid_value: Money,
    /// The products that sold the most in those now `COMPLETED`, by value, at most [`TOP`]
    pub top: Vec<TopProduct>,
}

/// A product as it sold in a period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopProduct {
    pub product_id: i64,
    /// As its latest line in the period has them
    pub name: String,
    pub unit: String,
    pub quantity: u64,
    /// The total of its lines
    pub value: Money,
}

/// A tap on "ยอดขาย", or on the card's button to see it again: only for the owner of an
/// approved shop (FR-SHOP-10).
pub async fn tapped(turn: &Turn<'_>, config: &Config) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let Some(shop) = &turn.actor.shop else {
        return Ok(vec![turn.templates.denied(Denied::NotAllowed)]);
    };
    let sales = gather(turn.db, config.app.time_zone, shop.id, turn.now).await?;
    Ok(vec![card(turn.templates, shop, &sales, turn.now)])
}

/// The card of `shop`'s `sales`, as they were `at`, with buttons to see them again and to see
/// the orders still to be paid for.
pub fn card(templates: &Templates, shop: &shop::Model, sales: &Sales, at: DateTimeUtc) -> Message {
    let buttons = vec![
        Item::Sales.postback().button(labels::REFRESH.clone()),
        lists::unpaid_list().button(labels::FILTER_UNPAID.clone()),
    ];
    templates.sales(&shop.name, sales, at, buttons)
}

/// The sales of shop `shop_id` at `now`, its days those of `time_zone`.
pub async fn gather(
    db: &impl ConnectionTrait,
    time_zone: Tz,
    shop_id: i64,
    now: DateTimeUtc,
) -> Result<Sales, DbErr> {
    let mut found = Vec::with_capacity(PERIODS.len());
    for days in PERIODS {
        let from = since(now, time_zone, days);
        let (unpaid, unpaid_value) = unpaid(db, shop_id, from).await?;
        found.push(Period {
            orders: periods::placed(db, days, from, Some(shop_id)).await?,
            unpaid,
            unpaid_value,
            top: top(db, shop_id, from).await?,
        });
    }
    Ok(Sales {
        periods: found.try_into().expect("one for each period"),
    })
}

/// How many of shop `shop_id`'s orders placed from `from` on are `COMPLETED` but not paid, and
/// their total.
async fn unpaid(
    db: &impl ConnectionTrait,
    shop_id: i64,
    from: DateTimeUtc,
) -> Result<(u64, Money), DbErr> {
    let found: Option<(i64, Option<Money>)> = order::Entity::find()
        .select_only()
        .column_as(order::Column::Id.count(), "orders")
        .column_as(order::Column::Total.sum(), "value")
        .filter(order::Column::ShopId.eq(shop_id))
        .filter(order::Column::CreatedAt.gte(from))
        .filter(order::Column::Status.eq(order::Status::Completed))
        .filter(order::Column::PaymentStatus.is_in(UNPAID))
        .into_tuple()
        .one(db)
        .await?;
    let (count, value) = found.unwrap_or_default();
    let count = count.try_into().expect("a count is never negative");
    Ok((count, value.unwrap_or(Money::ZERO)))
}

/// The products that sold the most in shop `shop_id`'s orders placed from `from` on and now
/// `COMPLETED`: by value, then by quantity, then the first listed.
async fn top(
    db: &impl ConnectionTrait,
    shop_id: i64,
    from: DateTimeUtc,
) -> Result<Vec<TopProduct>, DbErr> {
    let value = || order_item::Column::LineTotal.sum();
    let quantity = || order_item::Column::Quantity.sum();
    let rows: Vec<(i64, i64, Money, i64)> = order_item::Entity::find()
        .select_only()
        .column(order_item::Column::ProductId)
        .column_as(quantity(), "quantity")
        .column_as(value(), "value")
        .column_as(order_item::Column::Id.max(), "latest")
        .join(JoinType::InnerJoin, order_item::Relation::Order.def())
        .filter(order::Column::ShopId.eq(shop_id))
        .filter(order::Column::CreatedAt.gte(from))
        .filter(order::Column::Status.eq(order::Status::Completed))
        .group_by(order_item::Column::ProductId)
        .order_by(value(), Order::Desc)
        .order_by(quantity(), Order::Desc)
        .order_by_asc(order_item::Column::ProductId)
        .limit(TOP)
        .into_tuple()
        .all(db)
        .await?;
    let latest: HashMap<i64, order_item::Model> = order_item::Entity::find()
        .filter(order_item::Column::Id.is_in(rows.iter().map(|&(.., latest)| latest)))
        .all(db)
        .await?
        .into_iter()
        .map(|line| (line.id, line))
        .collect();
    let top = rows
        .into_iter()
        .map(|(product_id, quantity, value, latest_id)| {
            let line = &latest[&latest_id];
            TopProduct {
                product_id,
                name: line.name.clone(),
                unit: line.unit.clone(),
                quantity: quantity.try_into().expect("a quantity is at least 1"),
                value,
            }
        })
        .collect();
    Ok(top)
}
