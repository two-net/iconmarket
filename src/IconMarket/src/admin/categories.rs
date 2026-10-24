//! "หมวดหมู่" (FR-ADM-05): admins add, rename, reorder, deactivate and delete categories. The
//! order they set is the order buyers see in "ซื้อของ"; a deactivated category is hidden there
//! and from new listings ([`categories::active`]). A category that has or had products is
//! deactivated, never deleted: products are never removed (DR-04) and keep their category.
//! Every change is audited in the transaction that makes it (FR-ADM-08).
//!
//! Each button carries the ID of the category it acts on, and a move the neighbour it swaps
//! with, so a tap on an old card that no longer matches shows the list as it is now instead of
//! acting (FR-UI-13). A name is typed in a [flow](CategoryFlow) of one step, and a deletion is
//! confirmed ([`DeleteCategory`]). Category names aren't personal data, so the audit log keeps
//! them.

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::audit::{self, Entry, Target};
use crate::catalog::categories;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::pending::{self, Applied, Proposal, Stale, Why};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::Message;
use crate::notifications::templates::{CATEGORIES_PER_PAGE, CategoryCard, Templates, labels};
use crate::persistence::entity::{category, product};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// The actions of the buttons of "หมวดหมู่", which is [`Item::Categories`]. All but
/// [`ADD`] name their category in `c`.
pub const ADD: &str = "categories.add";
pub const RENAME: &str = "categories.rename";
/// Swap the category with the one above it, `n`
pub const UP: &str = "categories.up";
/// Swap the category with the one below it, `n`
pub const DOWN: &str = "categories.down";
pub const DEACTIVATE: &str = "categories.deactivate";
pub const ACTIVATE: &str = "categories.activate";
pub const DELETE: &str = "categories.delete";
const CATEGORY: &str = "c";
const NEIGHBOUR: &str = "n";
/// The page "ดูเพิ่มเติม" shows, on the postback of "หมวดหมู่"
const PAGE: &str = "page";

/// The longest name: one that fits a button or Quick Reply uncut (FR-SRCH-01)
const NAME_CHARS: usize = Label::MAX_CHARS;

/// The flow asking for a category's name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryFlow {
    naming: Naming,
}

/// What the name is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Naming {
    /// A new category, last in the order
    Add,
    /// Category `id`
    Rename(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Name,
}

impl Flow for CategoryFlow {
    const NAME: &'static str = "category";
    type Step = Step;
}

/// Deleting a category, once confirmed (FR-UI-13). It holds only the category's ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteCategory {
    category_id: i64,
}

impl Proposal for DeleteCategory {
    const KIND: &'static str = "category.delete";

    fn requires(&self) -> access::Action {
        access::Action::Administer
    }

    /// Deletes the category if it still exists and has no products.
    async fn apply(
        &self,
        _id: Uuid,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        // Locked, so a product can't be listed in it until this commits
        let Some(category) = category::Entity::find_by_id(self.category_id)
            .lock_exclusive()
            .one(txn)
            .await?
        else {
            return Ok(Applied::Changed);
        };
        if has_products(txn, category.id).await? {
            return Ok(Applied::Changed);
        }
        category::Entity::delete_by_id(category.id)
            .exec(txn)
            .await?;
        let name = json!(category.name);
        let entry = entry(
            turn,
            audit::Action::CategoryDeleted,
            category.id,
            [("name", name)],
        );
        audit::record(txn, turn.now, [entry]).await?;
        let deleted = turn.templates.category_deleted(&category.name);
        Ok(Applied::Done(vec![
            deleted,
            list(txn, turn.templates, 0, None).await?,
        ]))
    }

    /// Why it wasn't deleted: it has products now, or is gone; otherwise the list as it is,
    /// to an admin only.
    async fn current_state(stale: Stale<Self>, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
            return Ok(vec![turn.templates.denied(denied)]);
        }
        let id = stale.proposal.map(|proposal| proposal.category_id);
        if let (Why::Changed, Some(id)) = (stale.why, id) {
            return match category::Entity::find_by_id(id).one(turn.db).await? {
                Some(category) => Ok(vec![in_use(turn, &category)]),
                None => not_found(turn).await,
            };
        }
        Ok(vec![list(turn.db, turn.templates, 0, id).await?])
    }
}

/// A tap on "หมวดหมู่", or on one of its buttons.
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let action = postback.action();
    if action == Item::Categories.action() {
        let page = postback
            .int(PAGE)
            .and_then(|page| usize::try_from(page).ok())
            .unwrap_or(0);
        return Ok(vec![list(turn.db, turn.templates, page, None).await?]);
    }
    if action == ADD {
        return start(turn, Naming::Add).await;
    }
    let (Some(id), neighbour) = (postback.int(CATEGORY), postback.int(NEIGHBOUR)) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    match (action, neighbour) {
        (RENAME, _) => start(turn, Naming::Rename(id)).await,
        (UP, Some(neighbour)) => move_category(turn, id, Direction::Up, neighbour).await,
        (DOWN, Some(neighbour)) => move_category(turn, id, Direction::Down, neighbour).await,
        (DEACTIVATE, _) => set_active(turn, id, false).await,
        (ACTIVATE, _) => set_active(turn, id, true).await,
        (DELETE, _) => delete(turn, id).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// The name typed (FR-UI-12 step 3), saved at once. `None` for anything but text, so routing
/// goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    session: Session<CategoryFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Input::Text(text) = input else {
        return Ok(None);
    };
    let templates = turn.templates;
    // The admin role can be lost mid-flow (FR-ADM-01)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        let done = Answer::<CategoryFlow>::Done(vec![templates.denied(denied)]);
        return flow::answered(turn, done).await.map(Some);
    }
    let naming = session.draft.naming;
    let again = |message| async move {
        flow::answered(turn, Answer::<CategoryFlow>::Again(vec![message]))
            .await
            .map(Some)
    };
    let name = match name(text) {
        Ok(name) => name,
        Err(Refused::TooLong(max)) => return again(templates.text_too_long(max)).await,
        Err(Refused::Empty) => {
            return match prompt(turn, naming).await? {
                Some(prompt) => again(prompt).await,
                None => end_not_found(turn).await.map(Some),
            };
        }
    };

    // Locked, so that two admins naming categories at once can't take the same name or place
    let all = locked(turn.db).await?;
    if all
        .iter()
        .any(|category| category.name == name && naming != Naming::Rename(category.id))
    {
        return again(templates.category_name_taken(&name)).await;
    }
    let messages = match naming {
        Naming::Add => {
            let last = all.iter().map(|category| category.sort_order).max();
            let category = category::ActiveModel {
                name: Set(name.clone()),
                sort_order: Set(last.unwrap_or(0) + 1),
                is_active: Set(true),
                ..Default::default()
            }
            .insert(turn.db)
            .await?;
            let entry = entry(
                turn,
                audit::Action::CategoryAdded,
                category.id,
                [("name", json!(name))],
            );
            audit::record(turn.db, turn.now, [entry]).await?;
            vec![
                templates.category_added(&name),
                list(turn.db, templates, 0, Some(category.id)).await?,
            ]
        }
        Naming::Rename(id) => match all.iter().find(|category| category.id == id) {
            // Deleted by another admin meanwhile
            None => return end_not_found(turn).await.map(Some),
            Some(category) if category.name == name => {
                vec![list(turn.db, templates, 0, Some(id)).await?]
            }
            Some(category) => {
                category::Entity::update_many()
                    .col_expr(category::Column::Name, Expr::value(name.clone()))
                    .filter(category::Column::Id.eq(id))
                    .exec(turn.db)
                    .await?;
                let details = [("from", json!(category.name)), ("to", json!(name))];
                let entry = entry(turn, audit::Action::CategoryRenamed, id, details);
                audit::record(turn.db, turn.now, [entry]).await?;
                vec![
                    templates.category_renamed(&category.name, &name),
                    list(turn.db, templates, 0, Some(id)).await?,
                ]
            }
        },
    };
    flow::answered(turn, Answer::<CategoryFlow>::Done(messages))
        .await
        .map(Some)
}

/// Starts asking for the name `naming` needs, in place of any flow in progress.
async fn start(turn: &Turn<'_>, naming: Naming) -> Result<Vec<Message>, DbErr> {
    let Some(prompt) = prompt(turn, naming).await? else {
        return not_found(turn).await;
    };
    let session = Session {
        step: Step::Name,
        draft: CategoryFlow { naming },
    };
    flow::start(turn, session, vec![prompt]).await
}

/// What asks for the name `naming` needs, with the current name when renaming; `None` if the
/// category to rename is gone.
async fn prompt(turn: &Turn<'_>, naming: Naming) -> Result<Option<Message>, DbErr> {
    let current = match naming {
        Naming::Add => None,
        Naming::Rename(id) => match category::Entity::find_by_id(id).one(turn.db).await? {
            Some(category) => Some(category.name),
            None => return Ok(None),
        },
    };
    let ask = turn
        .templates
        .ask_category_name(NAME_CHARS, current.as_deref());
    Ok(Some(ask))
}

/// Why a name wasn't taken; the step asks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// Nothing but spaces
    Empty,
    /// Longer than [`NAME_CHARS`], in characters
    TooLong(usize),
}

/// `text` as a category's name: without spaces around it, and each run of spaces within it
/// made one, so that names that look alike are alike.
fn name(text: &str) -> Result<String, Refused> {
    let name = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match name.chars().count() {
        0 => Err(Refused::Empty),
        chars if chars > NAME_CHARS => Err(Refused::TooLong(NAME_CHARS)),
        _ => Ok(name),
    }
}

/// Which way a category moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

/// `order`, a list of category IDs, with `id` swapped with the category next to it in
/// `direction`, if that is still `neighbour`; `None` if the order changed since the button was
/// sent, which a second tap on the same button does too.
fn swapped(order: &[i64], id: i64, direction: Direction, neighbour: i64) -> Option<Vec<i64>> {
    let at = order.iter().position(|&category| category == id)?;
    let next_to = match direction {
        Direction::Up => at.checked_sub(1)?,
        Direction::Down => at + 1,
    };
    if order.get(next_to) != Some(&neighbour) {
        return None;
    }
    let mut order = order.to_vec();
    order.swap(at, next_to);
    Some(order)
}

/// Moves category `id` past `neighbour` in `direction`, and shows where it now is.
async fn move_category(
    turn: &Turn<'_>,
    id: i64,
    direction: Direction,
    neighbour: i64,
) -> Result<Vec<Message>, DbErr> {
    let all = locked(turn.db).await?;
    let order: Vec<_> = all.iter().map(|category| category.id).collect();
    let Some(moved) = swapped(&order, id, direction, neighbour) else {
        if !order.contains(&id) {
            return not_found(turn).await;
        }
        return Ok(vec![list(turn.db, turn.templates, 0, Some(id)).await?]);
    };
    // Numbered afresh from 1, so places are never shared; only the two swapped change
    // unless they were
    for (category, sort_order) in moved.iter().zip(1..) {
        let unchanged = all
            .iter()
            .any(|row| row.id == *category && row.sort_order == sort_order);
        if !unchanged {
            category::Entity::update_many()
                .col_expr(category::Column::SortOrder, Expr::value(sort_order))
                .filter(category::Column::Id.eq(*category))
                .exec(turn.db)
                .await?;
        }
    }
    let rank = |order: &[i64]| {
        order
            .iter()
            .position(|&category| category == id)
            .map(|at| at + 1)
    };
    let name = all
        .iter()
        .find(|category| category.id == id)
        .map(|category| json!(category.name));
    let details = [
        ("name", name.unwrap_or(Value::Null)),
        ("from", json!(rank(&order))),
        ("to", json!(rank(&moved))),
    ];
    let entry = entry(turn, audit::Action::CategoryMoved, id, details);
    audit::record(turn.db, turn.now, [entry]).await?;
    Ok(vec![list(turn.db, turn.templates, 0, Some(id)).await?])
}

/// Activates or deactivates category `id`, unless it already is so.
async fn set_active(turn: &Turn<'_>, id: i64, active: bool) -> Result<Vec<Message>, DbErr> {
    let Some(category) = category::Entity::find_by_id(id)
        .lock_exclusive()
        .one(turn.db)
        .await?
    else {
        return not_found(turn).await;
    };
    if category.is_active == active {
        // By an earlier tap, or another admin
        return Ok(vec![list(turn.db, turn.templates, 0, Some(id)).await?]);
    }
    category::Entity::update_many()
        .col_expr(category::Column::IsActive, Expr::value(active))
        .filter(category::Column::Id.eq(id))
        .exec(turn.db)
        .await?;
    let (action, done) = if active {
        let done = turn.templates.category_activated(&category.name);
        (audit::Action::CategoryActivated, done)
    } else {
        let done = turn.templates.category_deactivated(&category.name);
        (audit::Action::CategoryDeactivated, done)
    };
    let entry = entry(turn, action, id, [("name", json!(category.name))]);
    audit::record(turn.db, turn.now, [entry]).await?;
    Ok(vec![
        done,
        list(turn.db, turn.templates, 0, Some(id)).await?,
    ])
}

/// Asks to confirm deleting category `id`, or says why it can't be.
async fn delete(turn: &Turn<'_>, id: i64) -> Result<Vec<Message>, DbErr> {
    let Some(category) = category::Entity::find_by_id(id).one(turn.db).await? else {
        return not_found(turn).await;
    };
    if has_products(turn.db, id).await? {
        return Ok(vec![in_use(turn, &category)]);
    }
    let confirm = pending::propose(turn, &DeleteCategory { category_id: id }).await?;
    let confirm = confirm.button(labels::CONFIRM_DELETE.clone());
    Ok(vec![
        turn.templates
            .confirm_delete_category(&category.name, confirm),
    ])
}

/// Why `category` can't be deleted, with the button that deactivates it instead while it is
/// active.
fn in_use(turn: &Turn<'_>, category: &category::Model) -> Message {
    let deactivate = category.is_active.then(|| {
        let deactivate = Postback::new(DEACTIVATE).with_int(CATEGORY, category.id);
        deactivate.button(labels::DEACTIVATE.clone())
    });
    turn.templates.category_in_use(&category.name, deactivate)
}

/// Whether any product is or was listed in category `id`, deleted ones included: they keep
/// their category (DR-04).
async fn has_products(db: &impl ConnectionTrait, id: i64) -> Result<bool, DbErr> {
    let products = product::Entity::find()
        .filter(product::Column::CategoryId.eq(id))
        .count(db)
        .await?;
    Ok(products > 0)
}

/// Every category in order, locked until the event's transaction ends, so that changes by
/// admins acting at once apply one after the other.
async fn locked(db: &DatabaseTransaction) -> Result<Vec<category::Model>, DbErr> {
    categories::ordered().lock_exclusive().all(db).await
}

/// "หมวดหมู่" at page `page`, or at the page showing category `around` if it exists.
async fn list(
    db: &impl ConnectionTrait,
    templates: &Templates,
    page: usize,
    around: Option<i64>,
) -> Result<Message, DbErr> {
    let all = categories::ordered().all(db).await?;
    let products: HashMap<i64, i64> = product::Entity::find()
        .select_only()
        .column(product::Column::CategoryId)
        .column_as(product::Column::Id.count(), "products")
        .filter(product::Column::Status.ne(product::Status::Deleted))
        .group_by(product::Column::CategoryId)
        .into_tuple()
        .all(db)
        .await?
        .into_iter()
        .collect();

    let pages = all.len().div_ceil(CATEGORIES_PER_PAGE);
    let page = around
        .and_then(|id| all.iter().position(|category| category.id == id))
        .map_or(page, |at| at / CATEGORIES_PER_PAGE);
    let page = if page < pages { page } else { 0 };
    let cards: Vec<_> = all
        .iter()
        .enumerate()
        .map(|(at, category)| {
            let acts = |action| Postback::new(action).with_int(CATEGORY, category.id);
            let button = |action, label: &Label| acts(action).button(label.clone());
            let swap = |action, label: &Label, neighbour: &category::Model| {
                let swap = acts(action).with_int(NEIGHBOUR, neighbour.id);
                swap.button(label.clone())
            };
            let toggle = if category.is_active {
                button(DEACTIVATE, &labels::DEACTIVATE)
            } else {
                button(ACTIVATE, &labels::ACTIVATE)
            };
            CategoryCard {
                rank: at + 1,
                name: &category.name,
                is_active: category.is_active,
                products: products
                    .get(&category.id)
                    .map_or(0, |&count| u64::try_from(count).unwrap_or(0)),
                up: at
                    .checked_sub(1)
                    .map(|above| swap(UP, &labels::MOVE_UP, &all[above])),
                down: all
                    .get(at + 1)
                    .map(|below| swap(DOWN, &labels::MOVE_DOWN, below)),
                rename: button(RENAME, &labels::RENAME),
                toggle,
                delete: button(DELETE, &labels::DELETE),
            }
        })
        .collect();
    let more = |next| {
        let more = Item::Categories.postback().with_int(PAGE, next as i64);
        more.to_string()
    };
    let add = Postback::new(ADD).button(labels::ADD_CATEGORY.clone());
    Ok(templates.categories(&cards, page, more, add))
}

/// What a tap on a category that is gone gets: the list as it is now.
async fn not_found(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    Ok(vec![
        turn.templates.denied(Denied::NotFound),
        list(turn.db, turn.templates, 0, None).await?,
    ])
}

/// Ends the flow, the category it renames being gone.
async fn end_not_found(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    let messages = not_found(turn).await?;
    flow::answered(turn, Answer::<CategoryFlow>::Done(messages)).await
}

/// The audit entry of `action`, done by the admin acting to category `id`.
fn entry<const N: usize>(
    turn: &Turn<'_>,
    action: audit::Action,
    id: i64,
    details: [(&str, Value); N],
) -> Entry {
    Entry {
        actor_id: Some(turn.actor.user.id),
        action,
        target: Target::Category(id),
        reason: None,
        details: audit::details(details),
    }
}
