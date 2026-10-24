//! Roles (SRS §2.3), which are additive: every registered user is a buyer, the owner of an
//! `APPROVED` shop is a seller, and the users listed in `admin.lineUserIds` are admins.
//!
//! Only the admin role is stored (`users.is_admin`, FR-ADM-01); the seller role follows from
//! the shop's status. A user's rich menu has tabs for their roles (FR-UI-10), and
//! `users.menu_roles` records the roles of the one linked to them. A user whose roles changed,
//! however they changed, is therefore found by [`outdated_menus`] until their menu is relinked.

use std::collections::HashSet;

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::{Condition, Expr};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, QueryTrait, TransactionTrait, TryInsertResult,
};

use crate::admin::audit::{self, Entry, Target};
use crate::config::AdminConfig;
use crate::persistence::entity::shop;
use crate::persistence::entity::user::{self, MenuRoles};

/// Why the system gives and takes the admin role, as audited.
const LISTED: &str = "listed in admin.lineUserIds";
const UNLISTED: &str = "no longer listed in admin.lineUserIds";

/// The roles a user holds beyond buyer, which everyone is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub seller: bool,
    pub admin: bool,
}

impl Roles {
    /// The roles of `user`, whose shop, if they have one, is `shop`. A suspension takes no
    /// role away; the actions it forbids are refused by [`authorize`](super::access::authorize).
    pub fn of(user: &user::Model, shop: Option<&shop::Model>) -> Self {
        Self {
            seller: shop.is_some_and(|shop| {
                shop.owner_id == user.id && shop.status == shop::Status::Approved
            }),
            admin: user.is_admin,
        }
    }

    /// The rich menu for these roles: `None` for the default, buyer-only one.
    pub fn menu(self) -> Option<MenuRoles> {
        match (self.seller, self.admin) {
            (false, false) => None,
            (true, false) => Some(MenuRoles::Seller),
            (false, true) => Some(MenuRoles::Admin),
            (true, true) => Some(MenuRoles::SellerAdmin),
        }
    }
}

/// Grants the admin role to the users listed in `admins` and removes it from everyone else,
/// deleted accounts included, auditing each change as the system's. Returns how many were
/// granted and revoked.
///
/// Listed users who haven't followed the OA have no row yet; [`register`] gives them the role
/// when they do.
pub async fn sync_admins(
    txn: &DatabaseTransaction,
    admins: &AdminConfig,
    now: DateTimeUtc,
) -> Result<(u64, u64), DbErr> {
    let mut granted = user::Entity::update_many()
        .col_expr(user::Column::IsAdmin, Expr::value(true))
        .col_expr(user::Column::UpdatedAt, Expr::value(now))
        .filter(user::Column::IsAdmin.eq(false))
        .filter(user::Column::LineUserId.is_in(&admins.line_user_ids))
        .exec_with_returning(txn)
        .await?;
    let mut revoked = user::Entity::update_many()
        .col_expr(user::Column::IsAdmin, Expr::value(false))
        .col_expr(user::Column::UpdatedAt, Expr::value(now))
        .filter(user::Column::IsAdmin.eq(true))
        .filter(
            Condition::any()
                .add(user::Column::LineUserId.is_null())
                .add(user::Column::LineUserId.is_not_in(&admins.line_user_ids)),
        )
        .exec_with_returning(txn)
        .await?;
    // In ID order, so the log reads the same every time
    granted.sort_by_key(|user| user.id);
    revoked.sort_by_key(|user| user.id);
    let entries = granted
        .iter()
        .map(|user| system_entry(audit::Action::AdminGranted, user.id, LISTED))
        .chain(
            revoked
                .iter()
                .map(|user| system_entry(audit::Action::AdminRevoked, user.id, UNLISTED)),
        );
    audit::record(txn, now, entries).await?;
    Ok((granted.len() as u64, revoked.len() as u64))
}

/// The user with `line_user_id`, created at `now` on their first event (FR-ONB-01) as an
/// active, following buyer. A user listed in `admins` is created with the admin role, so a
/// listed user who follows after startup gets it then (FR-ADM-01).
///
/// An existing user is returned as they are; following again and the profile are the follow
/// handler's (#19).
pub async fn register(
    db: &DatabaseConnection,
    admins: &AdminConfig,
    line_user_id: &str,
    now: DateTimeUtc,
) -> Result<user::Model, DbErr> {
    let txn = db.begin().await?;
    let is_admin = admins.line_user_ids.iter().any(|id| id == line_user_id);
    let new = user::ActiveModel {
        line_user_id: Set(Some(line_user_id.to_owned())),
        is_admin: Set(is_admin),
        is_following: Set(true),
        status: Set(user::Status::Active),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    // The single-row `exec_with_returning` reports a conflict as an error, so take the rows
    let inserted = user::Entity::insert(new)
        .on_conflict_do_nothing_on([user::Column::LineUserId])
        .exec_with_returning_many(&txn)
        .await?;
    let created = match inserted {
        TryInsertResult::Inserted(rows) => rows.into_iter().next(),
        TryInsertResult::Conflicted | TryInsertResult::Empty => None,
    };
    let user = match created {
        Some(user) => {
            if user.is_admin {
                let entry = system_entry(audit::Action::AdminGranted, user.id, LISTED);
                audit::record(&txn, now, [entry]).await?;
            }
            user
        }
        None => user::Entity::find()
            .filter(user::Column::LineUserId.eq(line_user_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("user {line_user_id}")))?,
    };
    txn.commit().await?;
    Ok(user)
}

fn system_entry(action: audit::Action, user_id: i64, reason: &str) -> Entry {
    Entry {
        actor_id: None,
        action,
        target: Target::User(user_id),
        reason: Some(reason.to_owned()),
    }
}

/// A user whose linked rich menu was made for other roles than they hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outdated {
    pub user_id: i64,
    pub line_user_id: String,
    /// The menu their roles call for
    pub menu: Option<MenuRoles>,
}

/// The users following the OA whose rich menu doesn't match their roles (FR-UI-10), in ID
/// order: all of them, or only the user `only`. Unfollowed users are left until they follow
/// again, as LINE can't link them a menu.
pub async fn outdated_menus(
    db: &impl ConnectionTrait,
    only: Option<i64>,
) -> Result<Vec<Outdated>, DbErr> {
    let users = user::Entity::find()
        .filter(user::Column::IsFollowing.eq(true))
        .filter(user::Column::Status.ne(user::Status::Deleted))
        .apply_if(only, |query, id| query.filter(user::Column::Id.eq(id)))
        .order_by_asc(user::Column::Id)
        .all(db)
        .await?;
    let sellers: HashSet<i64> = shop::Entity::find()
        .select_only()
        .column(shop::Column::OwnerId)
        .filter(shop::Column::Status.eq(shop::Status::Approved))
        .apply_if(only, |query, id| query.filter(shop::Column::OwnerId.eq(id)))
        .into_tuple()
        .all(db)
        .await?
        .into_iter()
        .collect();
    let outdated = users
        .into_iter()
        .filter_map(|user| {
            let roles = Roles {
                seller: sellers.contains(&user.id),
                admin: user.is_admin,
            };
            let menu = roles.menu();
            (menu != user.menu_roles).then_some(Outdated {
                user_id: user.id,
                line_user_id: user.line_user_id?,
                menu,
            })
        })
        .collect();
    Ok(outdated)
}

/// Records that the rich menu linked to user `user_id` is now the one for `menu`, so they are
/// no longer outdated. The rich-menu module (#17) calls it once LINE has linked the menu.
pub async fn menu_linked(
    db: &impl ConnectionTrait,
    user_id: i64,
    menu: Option<MenuRoles>,
) -> Result<(), DbErr> {
    user::Entity::update_many()
        .set(user::ActiveModel {
            menu_roles: Set(menu),
            ..Default::default()
        })
        .filter(user::Column::Id.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_combination_of_roles_has_its_menu() {
        let menu = |seller, admin| Roles { seller, admin }.menu();
        assert_eq!(menu(false, false), None);
        assert_eq!(menu(true, false), Some(MenuRoles::Seller));
        assert_eq!(menu(false, true), Some(MenuRoles::Admin));
        assert_eq!(menu(true, true), Some(MenuRoles::SellerAdmin));
    }
}
