//! "ลบบัญชี" (FR-ONB-07, NFR-PRIV-03): the user deletes their account, which also withdraws
//! their consent. Help's topic of the same name offers it (FR-HELP-01), to suspended users too.
//!
//! It is refused, with how many orders are in the way, while any order the user placed or any
//! order at their shop is open (AC-13). Otherwise the user confirms (FR-UI-13), and in one
//! transaction their personal data is cleared: every field the entity models mark (DR-10) on
//! their row, their shop's and the orders they placed, their consent, their payment slips and
//! draft photos, the PromptPay QR codes their shop's buyers were sent, and the logs that hold
//! their LINE user ID. Their shop is closed for good and its products are deleted, both kept as
//! rows (DR-04), so their photos go with retention (§5.4). Their cart, guided flow, pending
//! actions, join-code attempts and AI memory go too. Their past orders stay, the other party
//! seeing them as [`DELETED_ACCOUNT`]; the shop keeps its name, so its buyers still recognise
//! them.
//!
//! The user's row stays as an anonymous `DELETED` one, which no LINE user ID leads to: should
//! the same LINE account write again, it is registered as someone new and onboarded afresh.
//! The rich menu linked to them for their roles is unlinked once the deletion commits.
//!
//! [`DELETED_ACCOUNT`]: crate::notifications::templates::DELETED_ACCOUNT

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait, Iterable, JoinType,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, RelationTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use super::DELETE_ACCOUNT;
use super::access::{self, authorize};
use crate::admin::audit;
use crate::dispatcher::Turn;
use crate::dispatcher::flow;
use crate::dispatcher::pending::{self, Applied, Proposal, Stale, Why};
use crate::dispatcher::postback::Postback;
use crate::line::Message;
use crate::notifications::templates::labels;
use crate::persistence::entity::image::OwnerType;
use crate::persistence::entity::{
    PersonalData, ai_message, cart_item, image, join_attempt, order, payment_slip, pending_action,
    product, push_log, shop, user, webhook_event,
};

/// What consenting and using the OA left on the user's row besides their personal data: their
/// consent, which deletion withdraws (NFR-PRIV-03), and what their roles and suspension were.
const WITHDRAWN: [user::Column; 6] = [
    user::Column::ConsentVersion,
    user::Column::ConsentedAt,
    user::Column::JoinedAt,
    user::Column::ProfileRefreshedAt,
    user::Column::SuspensionReason,
    user::Column::MenuRoles,
];

/// A tap on "ลบบัญชี": why the account can't be deleted yet, or the question whether to.
pub async fn tapped(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::MyData).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    ask(turn).await
}

/// Refuses while orders are open, or proposes deleting the account as it is now.
async fn ask(turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
    let shop = turn.actor.shop.as_ref();
    let shop_id = shop.map(|shop| shop.id);
    let open = Open::of(turn.db, turn.actor.user.id, shop_id).await?;
    if open.any() {
        let refused = turn
            .templates
            .account_deletion_refused(open.as_buyer, open.as_seller);
        return Ok(vec![refused]);
    }
    let confirm = pending::propose(turn, &DeleteAccount { shop_id }).await?;
    let confirm = confirm.button(labels::CONFIRM_DELETE.clone());
    let shop = shop.map(|shop| shop.name.as_str());
    Ok(vec![turn.templates.confirm_delete_account(shop, confirm)])
}

/// The open orders that keep an account from being deleted (FR-ONB-07).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Open {
    /// That the user placed
    as_buyer: u64,
    /// At their shop
    as_seller: u64,
}

impl Open {
    /// The open orders of user `user_id`, and of their shop `shop_id`.
    async fn of(
        db: &impl ConnectionTrait,
        user_id: i64,
        shop_id: Option<i64>,
    ) -> Result<Self, DbErr> {
        let open = || {
            let statuses = order::Status::iter().filter(|status| !status.is_final());
            order::Entity::find().filter(order::Column::Status.is_in(statuses))
        };
        let as_buyer = open()
            .filter(order::Column::BuyerId.eq(user_id))
            .count(db)
            .await?;
        let as_seller = match shop_id {
            Some(shop_id) => {
                open()
                    .filter(order::Column::ShopId.eq(shop_id))
                    .count(db)
                    .await?
            }
            None => 0,
        };
        Ok(Self {
            as_buyer,
            as_seller,
        })
    }

    fn any(self) -> bool {
        self.as_buyer > 0 || self.as_seller > 0
    }
}

/// Deleting the user's account, once confirmed (FR-UI-13). It holds only the ID of the shop
/// the confirmation named, if any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteAccount {
    shop_id: Option<i64>,
}

impl Proposal for DeleteAccount {
    const KIND: &'static str = "account.delete";

    fn requires(&self) -> access::Action {
        access::Action::MyData
    }

    /// Deletes the account, unless an order became open or the user's shop changed since the
    /// confirmation was shown.
    async fn apply(
        &self,
        _id: Uuid,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        // Locked in the order an admin's change to the account locks them
        let found = user::Entity::find_by_id(turn.actor.user.id)
            .filter(user::Column::Status.ne(user::Status::Deleted))
            .lock_exclusive()
            .one(txn)
            .await?;
        let Some(user) = found else {
            return Ok(Applied::Changed);
        };
        let shop = shop::Entity::find()
            .filter(shop::Column::OwnerId.eq(user.id))
            .lock_exclusive()
            .one(txn)
            .await?;
        let shop_id = shop.as_ref().map(|shop| shop.id);
        if shop_id != self.shop_id {
            return Ok(Applied::Changed);
        }
        // Locked in ID order, as checkout locks them: a checkout at the shop either commits
        // first, and its order is counted below, or waits and then finds them deleted
        if let Some(shop_id) = shop_id {
            product::Entity::find()
                .select_only()
                .column(product::Column::Id)
                .filter(product::Column::ShopId.eq(shop_id))
                .filter(product::Column::Status.ne(product::Status::Deleted))
                .order_by_asc(product::Column::Id)
                .lock_exclusive()
                .into_tuple::<i64>()
                .all(txn)
                .await?;
        }
        if Open::of(txn, user.id, shop_id).await?.any() {
            return Ok(Applied::Changed);
        }

        let products = delete(txn, turn.now, &user, shop.as_ref()).await?;
        let entry = audit::Entry {
            actor_id: Some(user.id),
            action: audit::Action::AccountDeleted,
            target: audit::Target::User(user.id),
            reason: None,
            details: audit::details([
                ("shop_id", json!(shop_id)),
                ("products_deleted", json!(products)),
            ]),
        };
        audit::record(txn, turn.now, [entry]).await?;
        // A seller's or admin's menu stays linked at LINE unless unlinked
        if let (Some(line_user_id), Some(_)) = (user.line_user_id, user.menu_roles) {
            turn.unlink_rich_menu(line_user_id);
        }
        info!(user_id = user.id, shop_id, products, "account deleted");
        Ok(Applied::Done(vec![turn.templates.account_deleted()]))
    }

    /// Why the account still can't be deleted, or the question asked again for it as it is
    /// now; or, the deletion having expired or been cancelled, that the account is kept.
    async fn current_state(stale: Stale<Self>, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::MyData).await? {
            return Ok(vec![turn.denied(denied)]);
        }
        match stale.why {
            Why::Changed => ask(turn).await,
            // Confirmed can't be: a deleted account's taps come from someone new
            Why::Gone | Why::Confirmed => {
                let again = Postback::new(DELETE_ACCOUNT).button(labels::DELETE_ACCOUNT.clone());
                Ok(vec![turn.templates.account_kept(again)])
            }
        }
    }
}

/// Deletes `user`'s account at `now`, with their shop `shop`: clears their personal data
/// everywhere it is held, closes the shop for good and deletes its products, and ends whatever
/// they had in progress. Returns how many products were deleted.
async fn delete(
    txn: &DatabaseTransaction,
    now: DateTimeUtc,
    user: &user::Model,
    shop: Option<&shop::Model>,
) -> Result<u64, DbErr> {
    let user_id = user.id;

    // The row stays, as their orders and the audit log refer to it
    let mut deleted = user::Entity::update_many().set(user::ActiveModel {
        status: Set(user::Status::Deleted),
        is_admin: Set(false),
        is_following: Set(false),
        updated_at: Set(now),
        ..Default::default()
    });
    for &column in user::Entity::FIELDS.iter().chain(&WITHDRAWN) {
        deleted = deleted.col_expr(column, Expr::null());
    }
    deleted
        .filter(user::Column::Id.eq(user_id))
        .exec(txn)
        .await?;

    let mut products = 0;
    if let Some(shop) = shop {
        let mut closed = shop::Entity::update_many().set(shop::ActiveModel {
            status: Set(shop::Status::Closed),
            is_open: Set(false),
            deleted_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        });
        for &column in shop::Entity::FIELDS {
            closed = closed.col_expr(column, Expr::null());
        }
        closed
            .filter(shop::Column::Id.eq(shop.id))
            .exec(txn)
            .await?;
        // The PromptPay QR codes its buyers were sent show its PromptPay ID
        let orders = order::Entity::find()
            .select_only()
            .column(order::Column::Id)
            .filter(order::Column::ShopId.eq(shop.id))
            .into_query();
        image::Entity::delete_many()
            .filter(image::Column::OwnerType.eq(OwnerType::PaymentQr))
            .filter(image::Column::OwnerId.in_subquery(orders))
            .exec(txn)
            .await?;
        products = product::Entity::update_many()
            .set(product::ActiveModel {
                status: Set(product::Status::Deleted),
                deleted_at: Set(Some(now)),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(product::Column::ShopId.eq(shop.id))
            .filter(product::Column::Status.ne(product::Status::Deleted))
            .exec(txn)
            .await?
            .rows_affected;
    }

    // The orders they placed keep their items and amounts, but not where they went
    let mut orders = order::Entity::update_many();
    for &column in order::Entity::FIELDS {
        orders = orders.col_expr(column, Expr::null());
    }
    orders
        .filter(order::Column::BuyerId.eq(user_id))
        .exec(txn)
        .await?;
    // Slips are sent by buyers, so only theirs hold the user's data
    let slips: Vec<i64> = payment_slip::Entity::find()
        .select_only()
        .column(payment_slip::Column::Id)
        .join(JoinType::InnerJoin, payment_slip::Relation::Order.def())
        .filter(order::Column::BuyerId.eq(user_id))
        .into_tuple()
        .all(txn)
        .await?;
    if !slips.is_empty() {
        image::Entity::delete_many()
            .filter(image::Column::OwnerType.eq(OwnerType::Slip))
            .filter(image::Column::OwnerId.is_in(slips.iter().copied()))
            .exec(txn)
            .await?;
        let mut cleared = payment_slip::Entity::update_many();
        for &column in payment_slip::Entity::FIELDS {
            cleared = cleared.col_expr(column, Expr::null());
        }
        cleared
            .filter(payment_slip::Column::Id.is_in(slips))
            .exec(txn)
            .await?;
    }
    image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(OwnerType::Draft))
        .filter(image::Column::OwnerId.eq(user_id))
        .exec(txn)
        .await?;

    // What they had in progress, the confirmation just consumed included
    flow::end(txn, user_id).await?;
    pending_action::Entity::delete_many()
        .filter(pending_action::Column::UserId.eq(user_id))
        .exec(txn)
        .await?;
    cart_item::Entity::delete_many()
        .filter(cart_item::Column::UserId.eq(user_id))
        .exec(txn)
        .await?;
    join_attempt::Entity::delete_many()
        .filter(join_attempt::Column::UserId.eq(user_id))
        .exec(txn)
        .await?;
    ai_message::Entity::delete_many()
        .filter(ai_message::Column::UserId.eq(user_id))
        .exec(txn)
        .await?;

    // The logs that hold their LINE user ID: the events they sent, but those still to handle,
    // this one among them, and the pushes to them
    if let Some(line_user_id) = &user.line_user_id {
        let handled = [webhook_event::Status::Done, webhook_event::Status::Failed];
        webhook_event::Entity::delete_many()
            .filter(webhook_event::Column::UserKey.eq(line_user_id))
            .filter(webhook_event::Column::Status.is_in(handled))
            .exec(txn)
            .await?;
        push_log::Entity::delete_many()
            .filter(push_log::Column::Recipient.eq(line_user_id))
            .exec(txn)
            .await?;
    }
    Ok(products)
}
