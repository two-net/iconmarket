//! Rating an order (FR-ORD-14): once an order is completed, its buyer rates it, once: 1 to 5
//! stars, and what they would tell the shop, if anything.
//!
//! "ให้คะแนน" is on the buyer's cards and notices of a completed order until it is rated. It
//! asks how many stars, as Quick Replies. The stars are kept as soon as they are tapped, so a
//! rating isn't lost to a comment left unwritten, and a second rating of the order, from an old
//! Quick Reply or two taps at once, is refused (DR-05). Then it asks what to tell the shop, at
//! most [`COMMENT_CHARS`] characters, or "ข้าม". The order's cards show its rating to its buyer
//! and its seller, and to an admin who finds it; nothing is pushed. A shop's card shows the
//! average of its ratings once it has [`SHOWN_FROM`] of them (FR-SHOP-09).

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter, QuerySelect,
    RelationTrait, TryInsertResult,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{ORDER_ID, RATE, STARS, actions, cart};
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{Action, Message, QuickReply};
use crate::notifications::templates::{self, OrderRating, ShopRating, labels};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{order, rating, shop};
use crate::users::access::{self, Denied, authorize};

/// The longest comment, in characters, as for an order's note
pub const COMMENT_CHARS: usize = 200;
/// How many ratings a shop has before its card shows their average
pub const SHOWN_FROM: u64 = 3;

/// The flow asking what the buyer would tell the shop, once the stars are kept (FR-ORD-14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingFlow {
    order_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// What to tell the shop, typed, or "ข้าม"
    Comment,
}

impl Flow for RatingFlow {
    const NAME: &'static str = "order_rating";
    type Step = Step;
}

/// The buyer's "ให้คะแนน" on `order` (FR-ORD-14).
pub(super) fn button(order: &order::Model) -> Action {
    Postback::new(RATE)
        .with_int(ORDER_ID, order.id)
        .button(labels::RATE_ORDER.clone())
}

/// The buyer's "ให้คะแนน" on order `id`, or the `stars` they tapped then (FR-ORD-14): asks how
/// many stars, or keeps them and asks what to tell the shop. Refused once the order is rated;
/// shows the order as it is if it isn't completed.
pub(super) async fn tapped(
    turn: &Turn<'_>,
    id: i64,
    stars: Option<i64>,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ActAsBuyer(id)).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let Some(order) = order::Entity::find_by_id(id).one(turn.db).await? else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    if order.status != order::Status::Completed {
        return Ok(vec![actions::now_is(
            turn.templates,
            &order,
            ActorType::Buyer,
        )]);
    }
    let (templates, number) = (turn.templates, &order.order_number);
    if let Some(rated) = of_order(turn.db, id).await? {
        return Ok(vec![templates.already_rated(
            number,
            rated.stars,
            carry_on(),
        )]);
    }
    let Some(stars) = stars else {
        let shop = shop::Entity::find_by_id(order.shop_id)
            .one(turn.db)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("shop of order {id}")))?;
        let stars = (1..=5_i16)
            .rev()
            .map(|stars| {
                Postback::new(RATE)
                    .with_int(ORDER_ID, id)
                    .with_int(STARS, stars.into())
                    .button(templates::stars_label(stars))
            })
            .collect();
        return Ok(vec![templates.ask_stars(number, &shop.name, stars)]);
    };
    let Some(stars) = i16::try_from(stars)
        .ok()
        .filter(|stars| (1..=5).contains(stars))
    else {
        return Ok(vec![templates.denied(Denied::NotFound)]);
    };
    let new = rating::ActiveModel {
        order_id: Set(id),
        stars: Set(stars),
        comment: Set(None),
        created_at: Set(turn.now),
        ..Default::default()
    };
    // The single-row `exec_with_returning` reports a conflict as an error, so take the rows:
    // none came back if a tap at the same moment rated it first
    let inserted = rating::Entity::insert(new)
        .on_conflict_do_nothing_on([rating::Column::OrderId])
        .exec_with_returning_many(turn.db)
        .await?;
    let kept = match inserted {
        TryInsertResult::Inserted(rows) => !rows.is_empty(),
        TryInsertResult::Conflicted | TryInsertResult::Empty => false,
    };
    if !kept {
        // Rated meanwhile, by a tap at the same moment
        let rated = of_order(turn.db, id)
            .await?
            .map_or(stars, |rated| rated.stars);
        return Ok(vec![templates.already_rated(number, rated, carry_on())]);
    }
    info!(order_id = id, stars, "order rated");
    let session = Session {
        step: Step::Comment,
        draft: RatingFlow { order_id: id },
    };
    let ask = templates.ask_rating_comment(number, stars, COMMENT_CHARS, skip());
    flow::start(turn, session, vec![ask]).await
}

/// What to tell the shop, typed, or "ข้าม" (FR-UI-12 step 3): kept with the stars, unless the
/// buyer may no longer act on the order (NFR-SEC-10). `None` for anything but text, so routing
/// goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    session: Session<RatingFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let done = |messages| async move {
        flow::answered(turn, Answer::<RatingFlow>::Done(messages))
            .await
            .map(Some)
    };
    let RatingFlow { order_id } = session.draft;
    let access = access::Action::ActAsBuyer(order_id);
    if let Err(denied) = authorize(turn.db, &turn.actor, access).await? {
        return done(vec![turn.denied(denied)]).await;
    }
    let order = order::Entity::find_by_id(order_id).one(turn.db).await?;
    let rated = of_order(turn.db, order_id).await?;
    let (Some(order), Some(rated)) = (order, rated) else {
        return done(vec![turn.templates.denied(Denied::NotFound)]).await;
    };
    let (templates, number) = (turn.templates, &order.order_number);
    let text = text.trim();
    if text == labels::SKIP.as_str() {
        return done(vec![templates.rating_saved(number, carry_on())]).await;
    }
    let again = match text.chars().count() {
        0 => Some(templates.ask_rating_comment(number, rated.stars, COMMENT_CHARS, skip())),
        chars if chars > COMMENT_CHARS => Some(
            templates
                .text_too_long(COMMENT_CHARS)
                .with_quick_reply(QuickReply::new([skip()])),
        ),
        _ => None,
    };
    if let Some(again) = again {
        let again = Answer::<RatingFlow>::Again(vec![again]);
        return flow::answered(turn, again).await.map(Some);
    }
    rating::Entity::update_many()
        .col_expr(rating::Column::Comment, Expr::value(text))
        .filter(rating::Column::OrderId.eq(order_id))
        .filter(rating::Column::Comment.is_null())
        .exec(turn.db)
        .await?;
    info!(order_id, "rating commented");
    done(vec![templates.rating_saved(number, carry_on())]).await
}

/// What an order's cards show of its rating `rated`.
pub(super) fn shown(rated: &rating::Model) -> OrderRating<'_> {
    OrderRating {
        stars: rated.stars,
        comment: rated.comment.as_deref(),
    }
}

/// The rating of order `id`, if it is rated.
pub async fn of_order(db: &impl ConnectionTrait, id: i64) -> Result<Option<rating::Model>, DbErr> {
    rating::Entity::find()
        .filter(rating::Column::OrderId.eq(id))
        .one(db)
        .await
}

/// The ratings of the orders `ids` that are rated, by order.
pub async fn of_orders(
    db: &impl ConnectionTrait,
    ids: impl IntoIterator<Item = i64>,
) -> Result<HashMap<i64, rating::Model>, DbErr> {
    let all = rating::Entity::find()
        .filter(rating::Column::OrderId.is_in(ids))
        .all(db)
        .await?;
    Ok(all
        .into_iter()
        .map(|rated| (rated.order_id, rated))
        .collect())
}

/// The average rating of each of shops `ids` that has at least [`SHOWN_FROM`] ratings, by shop
/// (FR-SHOP-09): those of its orders, in one query for a page of shops.
pub async fn of_shops(
    db: &impl ConnectionTrait,
    ids: &[i64],
) -> Result<HashMap<i64, ShopRating>, DbErr> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, i64, i64)> = rating::Entity::find()
        .select_only()
        .column(order::Column::ShopId)
        .column_as(rating::Column::Id.count(), "ratings")
        .column_as(rating::Column::Stars.sum(), "stars")
        .join(JoinType::InnerJoin, rating::Relation::Order.def())
        .filter(order::Column::ShopId.is_in(ids.iter().copied()))
        .group_by(order::Column::ShopId)
        .into_tuple()
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(shop_id, count, stars)| {
            let (count, stars) = (u64::try_from(count).ok()?, u64::try_from(stars).ok()?);
            (count >= SHOWN_FROM).then(|| (shop_id, average(count, stars)))
        })
        .collect())
}

/// The average of `count` ratings of `stars` in all, rounded half up to tenths.
fn average(count: u64, stars: u64) -> ShopRating {
    ShopRating {
        tenths: (20 * stars + count) / (2 * count),
        count,
    }
}

/// "ข้าม": goes without a comment.
fn skip() -> Action {
    Action::message(labels::SKIP.clone(), labels::SKIP.as_str())
}

/// Where the buyer goes once the rating is done: on shopping, or the menu.
fn carry_on() -> Vec<Action> {
    vec![cart::continue_shopping(), cart::menu_button()]
}
