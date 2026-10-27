//! "สินค้าของฉัน" (FR-PROD-05, FR-PROD-06): the owner of an approved shop sees its products, a
//! card each, and changes them: the stock, the price, the photos and the other parts, whether
//! buyers see them, or deletes them.
//!
//! The list pages the shop's products but the deleted ones (FR-SRCH-03): sold-out ones first,
//! then the newest. A product's stock is always what is left for new orders (FR-PROD-05). At 0
//! its card says "หมด" and its first button, "เติมของ", asks for the new stock, so a tap and the
//! number make it orderable again (FR-PROD-06).
//!
//! Each part is changed in a [flow](EditFlow) of one step that asks for it as "เพิ่มสินค้า"
//! does, showing what it is now, and saves it as soon as it is answered: that part alone, so a
//! checkout taking stock meanwhile is never undone (#32). A new price applies to new checkouts
//! only; orders keep what they were placed at (FR-PROD-08, DR-03). Photos are added to the
//! product's, up to `image.maxPerProduct`, or all replaced. Hiding and showing is a tap whose
//! button names the state it sets, so tapping it again changes nothing (FR-UI-13); deleting is
//! confirmed ([`DeleteProduct`]) and keeps the product, `DELETED` (DR-04). Each button carries
//! the product's ID, checked against the seller's shop at every tap. What the user types is
//! never logged (NFR-PRIV-05).
//!
//! A changed name, unit or description, or new photos, are [moderated](moderation) before they
//! are saved: found suspicious, the product is saved `HELD` for review, so buyers don't see it
//! until an admin lists it again (#31). Its other parts change as they are.

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Uuid;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use tracing::info;
use url::Url;

use super::{
    ProductFlow, Step, Taken, ask_category, choices, max_photos, may_become, photos_done_label,
    prompt, receive, restocks, take, why,
};
use crate::ai;
use crate::catalog::images::{self, Owner};
use crate::catalog::moderation::{self, Flag, Held, Subject};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::pending::{self, Applied, Proposal, Stale};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{self, Action, Message};
use crate::notifications::templates::{
    MyProductCard, PRODUCTS_PER_PAGE, Templates, labels, with_choices,
};
use crate::persistence::entity::{category, product};
use crate::thai::Label;
use crate::users::access::{self, Denied, authorize};

/// What the actions of the buttons of "สินค้าของฉัน" start with, but the list's own, which is
/// [`Item::MyProducts`]. All but [`CATEGORIES`] name their product in `id`.
pub const PREFIX: &str = "my_products.";
/// Changes the part named in `p`
pub const PART: &str = "my_products.part";
/// Asks which part to change
pub const EDIT: &str = "my_products.edit";
/// Shows the product to buyers if its `to` is 1, or hides it from them if 0
pub const SHOW: &str = "my_products.show";
pub const DELETE: &str = "my_products.delete";
/// The categories' Quick Replies from `page` on, while the category is being changed
pub const CATEGORIES: &str = "my_products.categories";
const PRODUCT: &str = "id";
const WHICH: &str = "p";
const TO: &str = "to";
/// The page "ดูเพิ่มเติม" shows, on the postback of "สินค้าของฉัน" or of [`CATEGORIES`]
const PAGE: &str = "page";

/// "สินค้าของฉัน" changing a part of a listed product: the product as it was when the part was
/// tapped, with the answer taken into it. Its draft's `product_id` is the product's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditFlow(ProductFlow);

impl Flow for EditFlow {
    const NAME: &'static str = "product_edit";
    type Step = Step;
}

/// Deleting a product, once confirmed (FR-UI-13). It holds only the product's ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteProduct {
    product_id: i64,
}

impl Proposal for DeleteProduct {
    const KIND: &'static str = "product.delete";

    fn requires(&self) -> access::Action {
        access::Action::ManageProduct(self.product_id)
    }

    /// Deletes the product, keeping it `DELETED` (DR-04), unless it already is. Its orders go
    /// on; its photos are deleted by retention, `retention.deletedImageDays` later.
    async fn apply(
        &self,
        _id: Uuid,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        let deleted = product::Entity::update_many()
            .set(product::ActiveModel {
                status: Set(product::Status::Deleted),
                deleted_at: Set(Some(turn.now)),
                updated_at: Set(turn.now),
                ..Default::default()
            })
            .filter(product::Column::Id.eq(self.product_id))
            .filter(product::Column::Status.ne(product::Status::Deleted))
            .exec_with_returning(txn)
            .await?;
        let Some(product) = deleted.into_iter().next() else {
            return Ok(Applied::Changed);
        };
        info!(product_id = product.id, "product deleted");
        let done = turn.templates.product_deleted(&product.name, true);
        Ok(Applied::Done(vec![done]))
    }

    /// Whether the product is deleted, by an earlier tap, or kept, the deletion having expired
    /// or been cancelled; to its seller only.
    async fn current_state(stale: Stale<Self>, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
            return Ok(vec![turn.templates.denied(denied)]);
        }
        let Some(id) = stale.proposal.map(|proposal| proposal.product_id) else {
            return Ok(vec![turn.templates.denied(Denied::NotFound)]);
        };
        if let Err(denied) =
            authorize(turn.db, &turn.actor, access::Action::ManageProduct(id)).await?
        {
            return Ok(vec![turn.templates.denied(denied)]);
        }
        let product = product::Entity::find_by_id(id).one(turn.db).await?;
        Ok(vec![match product {
            Some(product) => {
                let deleted = product.status == product::Status::Deleted;
                turn.templates.product_deleted(&product.name, deleted)
            }
            None => turn.templates.denied(Denied::NotFound),
        }])
    }
}

/// A tap on "สินค้าของฉัน", or on one of its buttons: only for the owner of an approved shop
/// (FR-SHOP-10), and only on one of its products that isn't deleted.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let Some(shop) = &turn.actor.shop else {
        return Ok(vec![turn.templates.denied(Denied::NotAllowed)]);
    };
    let action = postback.action();
    if action == Item::MyProducts.action() {
        let page = page(postback);
        return Ok(vec![
            list(turn.db, turn.templates, config, shop.id, page).await?,
        ]);
    }
    if action == CATEGORIES {
        return categories(turn, config, shop.id, page(postback)).await;
    }
    let Some(id) = postback.int(PRODUCT) else {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    };
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageProduct(id)).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let Some(product) = listed(turn.db, id).await? else {
        return not_found(turn, config, shop.id).await;
    };
    let part = postback.name(WHICH).and_then(Step::part);
    match (action, part, postback.int(TO)) {
        (PART, Some(part), _) if editable(&product) => start(turn, config, &product, part).await,
        (EDIT, ..) if editable(&product) => {
            let choose = turn.templates.choose_part(parts(&product));
            Ok(vec![turn.templates.editing_product(&product.name), choose])
        }
        (SHOW, _, Some(to @ (0 | 1))) => set_visible(turn, config, product, to == 1).await,
        (DELETE, ..) => {
            let proposal = DeleteProduct {
                product_id: product.id,
            };
            let confirm = pending::propose(turn, &proposal).await?;
            let confirm = confirm.button(labels::CONFIRM_DELETE.clone());
            Ok(vec![
                turn.templates
                    .confirm_delete_product(&product.name, confirm),
            ])
        }
        // A part no longer offered, as the product was removed meanwhile: its card as it is now
        (PART | EDIT, ..) => Ok(vec![card(turn.db, turn.templates, config, &product).await?]),
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// The page on `postback`, from 0.
fn page(postback: &Postback) -> usize {
    postback
        .int(PAGE)
        .and_then(|page| usize::try_from(page).ok())
        .unwrap_or(0)
}

/// Product `id`, unless it is deleted.
async fn listed(db: &impl ConnectionTrait, id: i64) -> Result<Option<product::Model>, DbErr> {
    product::Entity::find_by_id(id)
        .filter(product::Column::Status.ne(product::Status::Deleted))
        .one(db)
        .await
}

/// Whether the seller can change `product`'s parts: not once it is removed for good by an admin
/// (#31), when it can only be deleted.
fn editable(product: &product::Model) -> bool {
    matches!(
        product.status,
        product::Status::Active | product::Status::Hidden | product::Status::Held
    )
}

/// Whether `product` is for sale but sold out (FR-PROD-06).
fn sold_out(product: &product::Model) -> bool {
    product.status == product::Status::Active && product.stock == Some(0)
}

/// The buttons that change each part of `product`.
fn parts(product: &product::Model) -> Vec<Action> {
    Step::PARTS
        .into_iter()
        .map(|part| part_button(product, part, part.label()))
        .collect()
}

/// The button labelled `label` that changes `part` of `product`.
fn part_button(product: &product::Model, part: Step, label: &Label) -> Action {
    let postback = Postback::new(PART)
        .with_int(PRODUCT, product.id)
        .with_name(WHICH, part.key());
    postback.button(label.clone())
}

/// Starts changing `part` of `product`, in place of any flow in progress: the question, after
/// the product's name.
async fn start(
    turn: &Turn<'_>,
    config: &Config,
    product: &product::Model,
    part: Step,
) -> Result<Vec<Message>, DbErr> {
    let photos = images::all(turn.db, Owner::Product(product.id)).await?;
    let draft = ProductFlow::from_product(product, photos);
    let mut ask = vec![turn.templates.editing_product(&product.name)];
    ask.extend(prompt(turn, config, &draft, part).await?);
    let session = Session {
        step: part,
        draft: EditFlow(draft),
    };
    flow::start(turn, session, ask).await
}

/// "ดูเพิ่มเติม" under the categories, while the category is being changed: their page `page`.
/// A button of a change no longer in progress shows "สินค้าของฉัน" instead.
async fn categories(
    turn: &Turn<'_>,
    config: &Config,
    shop_id: i64,
    page: usize,
) -> Result<Vec<Message>, DbErr> {
    let session = turn.flow.as_ref().and_then(|flow| flow.read::<EditFlow>());
    let Some(Session {
        step,
        draft: EditFlow(draft),
    }) = session
    else {
        return Ok(vec![
            list(turn.db, turn.templates, config, shop_id, 0).await?,
        ]);
    };
    let ask = if step == Step::Category {
        ask_category(turn, config, &draft, page).await?
    } else {
        prompt(turn, config, &draft, step).await?
    };
    flow::answered(turn, Answer::<EditFlow>::Again(ask)).await
}

/// The answer to the part being changed (FR-UI-12 step 3). `None` when the step doesn't take
/// this kind of input, such as a photo where the price is asked for, so routing goes on
/// (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    line: &line::Client,
    session: Session<EditFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    let Session {
        step,
        draft: EditFlow(mut draft),
    } = session;
    let Some(id) = draft.product_id else {
        let done = Answer::<EditFlow>::Done(vec![turn.templates.denied(Denied::NotFound)]);
        return flow::answered(turn, done).await.map(Some);
    };
    // A suspension, of the seller or of the shop, ends it (NFR-SEC-10)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageProduct(id)).await? {
        let done = Answer::<EditFlow>::Done(vec![turn.templates.denied(denied)]);
        return flow::answered(turn, done).await.map(Some);
    }
    let text = match (step, input) {
        (Step::Photos, Input::Image { message_id }) => {
            return photo(turn, config, ai, line, draft, message_id)
                .await
                .map(Some);
        }
        (_, Input::Text(text)) => text,
        _ => return Ok(None),
    };
    let messages = match take(turn, &mut draft, step, text).await? {
        Ok(Taken::Changed) => save(turn, config, ai, draft, step).await?,
        Ok(Taken::Kept) => {
            let done = match listed(turn.db, id).await? {
                Some(product) => vec![
                    turn.templates.product_unchanged(),
                    card(turn.db, turn.templates, config, &product).await?,
                ],
                None => vec![turn.templates.denied(Denied::NotFound)],
            };
            flow::answered(turn, Answer::<EditFlow>::Done(done)).await?
        }
        // The photos removed, for others to be sent
        Ok(Taken::Again) => {
            let ask = prompt(turn, config, &draft, step).await?;
            let session = Session {
                step,
                draft: EditFlow(draft),
            };
            flow::answered(turn, Answer::Next(session, ask)).await?
        }
        Err(refused) => {
            // Why, with the step's choices; or, for nothing at all, the step's question again
            let again = match why(turn, &draft, refused) {
                Some(why) => with_choices(vec![why], choices(turn, config, &draft, step, 0).await?),
                None => prompt(turn, config, &draft, step).await?,
            };
            flow::answered(turn, Answer::<EditFlow>::Again(again)).await?
        }
    };
    Ok(Some(messages))
}

/// Takes the photo the user sent in `message_id` as one more of the product's, if it has room
/// for it (FR-PROD-04). The photos are saved once there are as many as a product may have, and
/// asked for again until then.
async fn photo(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    line: &line::Client,
    mut draft: ProductFlow,
    message_id: &str,
) -> Result<Vec<Message>, DbErr> {
    let max = max_photos(config);
    let refused = if draft.photos.len() >= max {
        // Not even downloaded
        Some(turn.templates.product_photos_full(max))
    } else {
        match receive(turn, config, line, message_id).await? {
            Ok(id) => {
                draft.photos.push(id);
                None
            }
            Err(rejected) => Some(Message::text(turn.templates.image_rejected(rejected))),
        }
    };
    if let Some(refused) = refused {
        let mut again = vec![refused];
        again.extend(prompt(turn, config, &draft, Step::Photos).await?);
        return flow::answered(turn, Answer::<EditFlow>::Again(again)).await;
    }
    if draft.photos.len() >= max {
        return save(turn, config, ai, draft, Step::Photos).await;
    }
    // The question says how many there are now
    let ask = prompt(turn, config, &draft, Step::Photos).await?;
    let session = Session {
        step: Step::Photos,
        draft: EditFlow(draft),
    };
    flow::answered(turn, Answer::Next(session, ask)).await
}

/// Saves `step` of the product as `draft` has it, and nothing else of it, then ends the flow with
/// the product's card. The product must still be listed, a new category still active and new
/// photos still there; a part left as it is writes nothing. Moderation may hold it for review
/// (FR-PROD-09).
async fn save(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    mut draft: ProductFlow,
    step: Step,
) -> Result<Vec<Message>, DbErr> {
    let id = draft.product_id.expect("a listed product's");
    let user_id = turn.actor.user.id;
    // Before anything is locked, as the AI may take a while
    let flag = moderate(turn, config, ai, &draft, step, id).await?;
    // The product and its photos change together, or not at all
    let txn = turn.db.begin().await?;
    // Locked, so that a checkout or a deletion at the same time waits for this
    let Some(product) = product::Entity::find_by_id(id)
        .filter(product::Column::Status.ne(product::Status::Deleted))
        .lock_exclusive()
        .one(&txn)
        .await?
    else {
        txn.rollback().await?;
        let shop = turn
            .actor
            .shop
            .as_ref()
            .expect("authorized for its product");
        let messages = not_found(turn, config, shop.id).await?;
        return flow::answered(turn, Answer::<EditFlow>::Done(messages)).await;
    };
    let change = changed(&draft, step, &product);
    if let (Step::Category, Some(_), Some(category_id)) = (step, &change, draft.category_id) {
        // Locked, so it can't be deactivated until the product is in it (FR-PROD-03)
        let category = category::Entity::find_by_id(category_id)
            .lock_shared()
            .one(&txn)
            .await?;
        if !category.is_some_and(|category| category.is_active) {
            txn.rollback().await?;
            draft.category_id = Some(product.category_id);
            let mut again = vec![turn.templates.category_inactive()];
            again.extend(prompt(turn, config, &draft, Step::Category).await?);
            return flow::answered(turn, Answer::<EditFlow>::Again(again)).await;
        }
    }
    let mut photos_changed = false;
    if step == Step::Photos {
        let owner = Owner::Product(id);
        photos_changed = images::all(&txn, owner).await? != draft.photos;
        if photos_changed && !images::replace_all(&txn, owner, &draft.photos, user_id).await? {
            // The product's photos stay as they were; those sent are asked for again
            txn.rollback().await?;
            for id in std::mem::take(&mut draft.photos) {
                images::delete_draft(turn.db, id, user_id).await?;
            }
            draft.photos = images::all(turn.db, owner).await?;
            let mut again = vec![
                turn.templates
                    .product_photos_lost(photos_done_label(&draft)),
            ];
            again.extend(prompt(turn, config, &draft, Step::Photos).await?);
            let session = Session {
                step: Step::Photos,
                draft: EditFlow(draft),
            };
            return flow::answered(turn, Answer::Next(session, again)).await;
        }
    }
    if change.is_none() && !photos_changed {
        txn.rollback().await?;
        let unchanged = turn.templates.product_unchanged();
        let card = card(turn.db, turn.templates, config, &product).await?;
        return flow::answered(turn, Answer::<EditFlow>::Done(vec![unchanged, card])).await;
    }
    let mut update = product::ActiveModel {
        updated_at: Set(turn.now),
        ..change.unwrap_or_default()
    };
    if let Set(stock) = update.stock
        && restocks(product.stock, stock)
    {
        update.restocked_at = Set(Some(turn.now));
    }
    // Unless it was held, or removed, meanwhile
    let flag = flag.filter(|_| may_become(product.status, product::Status::Held));
    if flag.is_some() {
        update.status = Set(product::Status::Held);
    }
    let saved = product::Entity::update_many()
        .set(update)
        .filter(product::Column::Id.eq(id))
        .exec_with_returning(&txn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| DbErr::RecordNotFound(format!("product {id}")))?;
    txn.commit().await?;

    info!(
        product_id = id,
        part = step.key(),
        held = flag.is_some(),
        "product changed"
    );
    let templates = turn.templates;
    if let Some(flag) = &flag {
        let shop = turn
            .actor
            .shop
            .as_ref()
            .expect("authorized for its product");
        let held = Held::Product {
            product: &saved,
            shop,
        };
        moderation::hold(turn, held, flag).await?;
    }
    let done = match step {
        _ if flag.is_some() => templates.product_held_saved(),
        Step::Price => templates.product_price_saved(saved.price, &saved.unit),
        Step::Stock => templates.product_stock_saved(saved.stock, &saved.unit, sold_out(&product)),
        _ => templates.product_saved(),
    };
    let card = card(turn.db, templates, config, &saved).await?;
    flow::answered(turn, Answer::<EditFlow>::Done(vec![done, card])).await
}

/// Moderates what saving `step` as `draft` has changes of product `id` (FR-PROD-09), if that is
/// its text or new photos: why to hold it, if it should be. The product's text is checked as it
/// will be, and only the photos new to it. One held already isn't checked again.
async fn moderate(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    draft: &ProductFlow,
    step: Step,
    id: i64,
) -> Result<Option<Flag>, DbErr> {
    if !matches!(
        step,
        Step::Name | Step::Unit | Step::Description | Step::Photos
    ) {
        return Ok(None);
    }
    let Some(product) = listed(turn.db, id)
        .await?
        .filter(|product| product.status != product::Status::Held)
    else {
        return Ok(None);
    };
    let photos = if step == Step::Photos {
        let had = images::all(turn.db, Owner::Product(id)).await?;
        let new: Vec<Uuid> = draft
            .photos
            .iter()
            .filter(|photo| !had.contains(photo))
            .copied()
            .collect();
        if new.is_empty() {
            // Photos taken away only
            return Ok(None);
        }
        new
    } else if changed(draft, step, &product).is_some() {
        Vec::new()
    } else {
        return Ok(None);
    };
    let (mut name, mut unit, mut description) = (
        product.name.as_str(),
        product.unit.as_str(),
        product.description.as_deref(),
    );
    match step {
        Step::Name => name = draft.name.as_deref().unwrap_or(name),
        Step::Unit => unit = draft.unit.as_deref().unwrap_or(unit),
        Step::Description => description = draft.description.as_deref(),
        _ => {}
    }
    let subject = Subject::product(name, unit, description, photos);
    moderation::check(turn.db, config, ai, &subject).await
}

/// What saving `step` of `product` as `draft` has it changes, if it changes anything: that part
/// alone. The photos aren't a column; see [`images::replace_all`].
fn changed(
    draft: &ProductFlow,
    step: Step,
    product: &product::Model,
) -> Option<product::ActiveModel> {
    let change = product::ActiveModel::default();
    match step {
        Step::Name => draft
            .name
            .clone()
            .filter(|name| *name != product.name)
            .map(|name| product::ActiveModel {
                name: Set(name),
                ..change
            }),
        Step::Price => draft
            .price
            .filter(|&price| price != product.price)
            .map(|price| product::ActiveModel {
                price: Set(price),
                ..change
            }),
        Step::Unit => draft
            .unit
            .clone()
            .filter(|unit| *unit != product.unit)
            .map(|unit| product::ActiveModel {
                unit: Set(unit),
                ..change
            }),
        Step::Category => draft
            .category_id
            .filter(|&id| id != product.category_id)
            .map(|id| product::ActiveModel {
                category_id: Set(id),
                ..change
            }),
        // Not what it was: what is left now, whatever orders took meanwhile (FR-PROD-05)
        Step::Stock => draft
            .stock
            .map(super::Stock::column)
            .filter(|&stock| stock != product.stock)
            .map(|stock| product::ActiveModel {
                stock: Set(stock),
                ..change
            }),
        Step::Description => {
            (draft.description != product.description).then(|| product::ActiveModel {
                description: Set(draft.description.clone()),
                ..change
            })
        }
        Step::Photos | Step::Summary => None,
    }
}

/// Shows `product` to buyers or hides it from them (FR-PROD-05, FR-PROD-07), unless it already
/// is so; held or removed, it stays as it is. Then its card.
async fn set_visible(
    turn: &Turn<'_>,
    config: &Config,
    product: product::Model,
    visible: bool,
) -> Result<Vec<Message>, DbErr> {
    use product::Status::{Active, Hidden};
    let (from, to) = if visible {
        (Hidden, Active)
    } else {
        (Active, Hidden)
    };
    debug_assert!(super::may_become(from, to));
    let changed = product::Entity::update_many()
        .set(product::ActiveModel {
            status: Set(to),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(product::Column::Id.eq(product.id))
        .filter(product::Column::Status.eq(from))
        .exec_with_returning(turn.db)
        .await?
        .into_iter()
        .next();
    let was_changed = changed.is_some();
    let product = match changed {
        Some(product) => {
            info!(product_id = product.id, visible, "product shown or hidden");
            product
        }
        // Changed meanwhile, or by an earlier tap
        None => match listed(turn.db, product.id).await? {
            Some(product) => product,
            None => {
                return not_found(turn, config, product.shop_id).await;
            }
        },
    };
    let mut messages = Vec::new();
    // A held or removed product's card says why it isn't shown
    if product.status == to {
        let done = turn
            .templates
            .product_visibility(&product.name, visible, was_changed);
        messages.push(done);
    }
    messages.push(card(turn.db, turn.templates, config, &product).await?);
    Ok(messages)
}

/// What a tap on a product that is gone gets: "สินค้าของฉัน" of shop `shop_id` as it is now.
async fn not_found(turn: &Turn<'_>, config: &Config, shop_id: i64) -> Result<Vec<Message>, DbErr> {
    Ok(vec![
        turn.templates.denied(Denied::NotFound),
        list(turn.db, turn.templates, config, shop_id, 0).await?,
    ])
}

/// "สินค้าของฉัน" of shop `shop_id` at page `page`: its products but the deleted ones, sold-out
/// ones first, to restock (FR-PROD-06), then the newest.
async fn list(
    db: &impl ConnectionTrait,
    templates: &Templates,
    config: &Config,
    shop_id: i64,
    page: usize,
) -> Result<Message, DbErr> {
    let mut products = product::Entity::find()
        .filter(product::Column::ShopId.eq(shop_id))
        .filter(product::Column::Status.ne(product::Status::Deleted))
        .order_by_desc(product::Column::Id)
        .all(db)
        .await?;
    // Stable, so the newest stay first among the sold-out ones and among the others
    products.sort_by_key(|product| !sold_out(product));
    let ids: Vec<i64> = products.iter().map(|product| product.id).collect();
    let photos = images::of_products(db, &ids).await?;
    let names: HashMap<i64, String> = category::Entity::find()
        .select_only()
        .column(category::Column::Id)
        .column(category::Column::Name)
        .into_tuple()
        .all(db)
        .await?
        .into_iter()
        .collect();

    let base = &config.app.base_url;
    let urls: Vec<Option<Url>> = products
        .iter()
        .map(|product| {
            let first = photos.get(&product.id).and_then(|photos| photos.first());
            first.map(|&id| images::product_image_url(base, id))
        })
        .collect();
    let cards: Vec<_> = products
        .iter()
        .zip(&urls)
        .map(|(product, url)| {
            let category = names.get(&product.category_id).map_or("", String::as_str);
            let photos = photos.get(&product.id).map_or(0, Vec::len);
            my_card(product, category, url.as_ref(), photos)
        })
        .collect();
    // A page past the last, from a button sent before products were deleted, starts over
    let pages = cards.len().div_ceil(PRODUCTS_PER_PAGE);
    let page = if page < pages { page } else { 0 };
    let more = |next: usize| {
        let next = i64::try_from(next).expect("a few pages");
        Item::MyProducts.postback().with_int(PAGE, next).to_string()
    };
    let add = Item::AddProduct
        .postback()
        .button(labels::ADD_PRODUCT.clone());
    Ok(templates.my_products(&cards, page, more, add))
}

/// `product`'s card as it is now, with its buttons.
async fn card(
    db: &impl ConnectionTrait,
    templates: &Templates,
    config: &Config,
    product: &product::Model,
) -> Result<Message, DbErr> {
    let photos = images::all(db, Owner::Product(product.id)).await?;
    let category = category::Entity::find_by_id(product.category_id)
        .one(db)
        .await?;
    let photo = photos
        .first()
        .map(|&id| images::product_image_url(&config.app.base_url, id));
    let category = category.as_ref().map_or("", |category| &category.name);
    let card = my_card(product, category, photo.as_ref(), photos.len());
    Ok(templates.my_product(&card))
}

/// What `product`'s card shows, in `category`, with `photos` photos, the first at `photo`, and
/// a button for each thing its seller can do with it now: "เติมของ" first while it is sold out
/// (FR-PROD-06).
fn my_card<'a>(
    product: &'a product::Model,
    category: &'a str,
    photo: Option<&'a Url>,
    photos: usize,
) -> MyProductCard<'a> {
    let acts = |action| Postback::new(action).with_int(PRODUCT, product.id);
    let mut buttons = Vec::new();
    let editable = editable(product);
    if editable {
        let stock = if product.stock == Some(0) {
            &labels::RESTOCK
        } else {
            &labels::EDIT_STOCK
        };
        buttons.push(vec![
            part_button(product, Step::Stock, stock),
            part_button(product, Step::Price, &labels::EDIT_PRICE),
        ]);
        buttons.push(vec![
            part_button(product, Step::Photos, &labels::CHANGE_PHOTOS),
            acts(EDIT).button(labels::EDIT.clone()),
        ]);
    }
    // Only listed and hidden products go between the two (FR-PROD-07)
    let mut last = Vec::new();
    match product.status {
        product::Status::Active => {
            last.push(
                acts(SHOW)
                    .with_int(TO, 0)
                    .button(labels::HIDE_PRODUCT.clone()),
            );
        }
        product::Status::Hidden => {
            last.push(
                acts(SHOW)
                    .with_int(TO, 1)
                    .button(labels::SHOW_PRODUCT.clone()),
            );
        }
        _ => {}
    }
    last.push(acts(DELETE).button(labels::DELETE.clone()));
    buttons.push(last);
    MyProductCard {
        name: &product.name,
        price: product.price,
        unit: &product.unit,
        category,
        stock: product.stock,
        status: product.status,
        photo,
        photos,
        buttons,
        primary: editable && product.stock == Some(0),
    }
}
