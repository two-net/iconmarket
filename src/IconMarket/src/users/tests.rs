//! Profiles, roles and authorization (§2.3, §7.4). The `async` tests each start a throwaway
//! PostgreSQL, so they need Docker.

use std::slice;

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::Json;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, IntoActiveModel, QueryOrder,
    TransactionTrait,
};
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::access::{Action, Actor, Denied, authorize};
use super::roles::{self, Outdated, Roles};
use super::*;
use crate::config::AdminConfig;
use crate::persistence::entity::user::MenuRoles;
use crate::persistence::entity::{audit_log, order, shop, user};
use crate::persistence::testing::{self, Market, at, market, new_order, new_shop, new_user};
use crate::persistence::{orm, random_id};

#[test]
fn profiles_are_refreshed_at_most_once_a_day() {
    let now = Utc.with_ymd_and_hms(2026, 10, 22, 7, 30, 0).unwrap();
    assert!(profile_refresh_due(None, now));
    assert!(!profile_refresh_due(Some(now), now));
    assert!(!profile_refresh_due(Some(now - TimeDelta::hours(23)), now));
    assert!(profile_refresh_due(Some(now - TimeDelta::hours(24)), now));
    assert!(profile_refresh_due(Some(now - TimeDelta::days(30)), now));
}

/// FR-UI-12, FR-ONB-02, FR-ONB-03, NFR-PRIV-01: consent to the current notice, then the join
/// code if one is set.
#[test]
fn onboarding_is_consent_then_the_join_code() {
    let mut config = crate::config::sample();
    let at = at();
    let mut user = user::Model {
        id: 1,
        line_user_id: Some("U0123456789abcdef0123456789abcdef".to_owned()),
        display_name: None,
        picture_url: None,
        profile_refreshed_at: None,
        is_admin: false,
        menu_roles: None,
        is_following: true,
        status: user::Status::Active,
        consent_version: None,
        consented_at: None,
        joined_at: None,
        zone: None,
        house_number: None,
        landmark_note: None,
        phone: None,
        latitude: None,
        longitude: None,
        created_at: at,
        updated_at: at,
    };
    let pending = |user: &user::Model, config: &crate::config::Config| {
        pending(user, &config.privacy, &config.village)
    };

    assert_eq!(pending(&user, &config), Some(Pending::Consent));
    user.consent_version = Some(config.privacy.notice_version.clone());
    assert_eq!(pending(&user, &config), None);

    // A code set later is asked for from everyone who hasn't entered one
    config.village.join_code = Some("ICON2569".to_owned());
    assert_eq!(pending(&user, &config), Some(Pending::JoinCode));
    user.joined_at = Some(at);
    assert_eq!(pending(&user, &config), None);

    // A new notice is asked for again, before anything else
    config.privacy.notice_version = "2".to_owned();
    assert_eq!(pending(&user, &config), Some(Pending::Consent));
    user.joined_at = None;
    assert_eq!(pending(&user, &config), Some(Pending::Consent));
}

async fn database() -> (ContainerAsync<Postgres>, DatabaseConnection) {
    let (container, pool) = testing::migrated().await;
    (container, orm(pool))
}

/// The answer to user `user_id` asking for `action`, loading them afresh as for each event.
async fn check(db: &DatabaseConnection, user_id: i64, action: Action) -> Result<(), Denied> {
    let actor = Actor::load(db, user_id).await.unwrap().expect("a user");
    authorize(db, &actor, action).await.unwrap()
}

async fn new_admin(db: &DatabaseConnection, line_user_id: &str) -> user::Model {
    user::ActiveModel {
        is_admin: Set(true),
        ..new_user(line_user_id)
    }
    .insert(db)
    .await
    .unwrap()
}

async fn set_user_status(db: &DatabaseConnection, user_id: i64, status: user::Status) {
    let mut user = user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    user.status = Set(status);
    user.update(db).await.unwrap();
}

async fn set_shop_status(db: &DatabaseConnection, shop_id: i64, status: shop::Status) {
    let mut shop = shop::Entity::find_by_id(shop_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    shop.status = Set(status);
    shop.update(db).await.unwrap();
}

async fn roles_of(db: &DatabaseConnection, user_id: i64) -> Roles {
    let actor = Actor::load(db, user_id).await.unwrap().expect("a user");
    actor.roles()
}

async fn outdated(db: &DatabaseConnection, only: Option<i64>) -> Vec<Outdated> {
    roles::outdated_menus(db, only).await.unwrap()
}

/// A pending order from the market's buyer at its shop.
async fn new_pending_order(db: &DatabaseConnection, market: &Market) -> order::Model {
    new_order(
        market.buyer.id,
        market.shop.id,
        "IM-20261022-0001",
        random_id(),
    )
    .insert(db)
    .await
    .unwrap()
}

/// Every audit row as (actor, action, target, details), oldest first.
async fn audited(db: &DatabaseConnection) -> Vec<(Option<i64>, String, Option<i64>, Json)> {
    audit_log::Entity::find()
        .order_by_asc(audit_log::Column::Id)
        .all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.actor_id, row.action, row.target_id, row.details))
        .collect()
}

/// NFR-SEC-02, NFR-MNT-07: only a shop's owner can accept its orders.
#[tokio::test]
async fn a_non_owner_cannot_accept_another_shops_order() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let order = new_pending_order(&db, &market).await;
    let seller = market.shop.owner_id;
    let other_seller = new_user("U-other-seller").insert(&db).await.unwrap();
    new_shop(other_seller.id).insert(&db).await.unwrap();
    let admin = new_admin(&db, "U-admin").await;
    let accept = Action::HandleOrder(order.id);

    assert_eq!(check(&db, seller, accept).await, Ok(()));
    // Another seller, the buyer and even an admin can't: admins cancel, they don't accept
    for user_id in [other_seller.id, market.buyer.id, admin.id] {
        assert_eq!(check(&db, user_id, accept).await, Err(Denied::NotAllowed));
    }
    assert_eq!(
        check(&db, seller, Action::HandleOrder(order.id + 1)).await,
        Err(Denied::NotFound)
    );

    // Only its buyer acts as its buyer
    let cancel = Action::ActAsBuyer(order.id);
    assert_eq!(check(&db, market.buyer.id, cancel).await, Ok(()));
    for user_id in [seller, other_seller.id, admin.id] {
        assert_eq!(check(&db, user_id, cancel).await, Err(Denied::NotAllowed));
    }

    // Its buyer, its seller and admins see it; no one else does
    let view = Action::ViewOrder(order.id);
    for user_id in [market.buyer.id, seller, admin.id] {
        assert_eq!(check(&db, user_id, view).await, Ok(()));
    }
    assert_eq!(
        check(&db, other_seller.id, view).await,
        Err(Denied::NotAllowed)
    );
}

/// NFR-SEC-10, FR-ADM-04, NFR-MNT-07: a suspension applies to buttons sent before it.
#[tokio::test]
async fn a_suspended_users_old_postback_is_refused() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let order = new_pending_order(&db, &market).await;
    let (seller, buyer) = (market.shop.owner_id, market.buyer.id);
    // The seller's order card and the buyer's cancel button, as sent with the order
    let accept = Action::HandleOrder(order.id);
    let cancel = Action::ActAsBuyer(order.id);
    assert_eq!(check(&db, seller, accept).await, Ok(()));
    assert_eq!(check(&db, buyer, cancel).await, Ok(()));

    set_user_status(&db, seller, user::Status::Suspended).await;
    set_user_status(&db, buyer, user::Status::Suspended).await;

    // The same taps are refused: they can't order, sell or use the AI
    let refused = [
        (seller, accept),
        (seller, Action::ManageShop),
        (seller, Action::ManageProduct(market.product.id)),
        (buyer, cancel),
        (buyer, Action::Buy),
        (buyer, Action::UseAi),
        (buyer, Action::Report),
        (buyer, Action::ApplyForShop),
    ];
    for (user_id, action) in refused {
        assert_eq!(
            check(&db, user_id, action).await,
            Err(Denied::Suspended),
            "{action:?}"
        );
    }
    // They still see their orders and help, and keep their data rights
    for user_id in [seller, buyer] {
        for action in [
            Action::ViewOrder(order.id),
            Action::Help,
            Action::Browse,
            Action::MyData,
        ] {
            assert_eq!(check(&db, user_id, action).await, Ok(()), "{action:?}");
        }
    }
    // A suspension takes no role away; it only refuses what the role would allow
    assert!(roles_of(&db, seller).await.seller);

    // Unsuspended, they carry on
    set_user_status(&db, seller, user::Status::Active).await;
    assert_eq!(check(&db, seller, accept).await, Ok(()));
}

/// FR-SHOP-10: a suspended shop's products and settings are locked, but its orders continue.
#[tokio::test]
async fn a_suspended_shop_keeps_its_orders_but_not_its_catalog() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let order = new_pending_order(&db, &market).await;
    let seller = market.shop.owner_id;
    let product = Action::ManageProduct(market.product.id);
    assert_eq!(check(&db, seller, Action::ManageShop).await, Ok(()));
    assert_eq!(check(&db, seller, product).await, Ok(()));

    set_shop_status(&db, market.shop.id, shop::Status::Suspended).await;
    assert_eq!(
        check(&db, seller, Action::ManageShop).await,
        Err(Denied::ShopSuspended)
    );
    assert_eq!(
        check(&db, seller, product).await,
        Err(Denied::ShopSuspended)
    );
    assert_eq!(
        check(&db, seller, Action::HandleOrder(order.id)).await,
        Ok(())
    );
    // Buying elsewhere is unaffected
    assert_eq!(check(&db, seller, Action::Buy).await, Ok(()));
}

/// §2.3: a seller owns an approved shop; nothing else makes one.
#[tokio::test]
async fn only_an_approved_shop_makes_a_seller() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let seller = market.shop.owner_id;

    let cases = [
        (shop::Status::Approved, true, Ok(())),
        (shop::Status::Pending, false, Err(Denied::NotAllowed)),
        (shop::Status::Rejected, false, Err(Denied::NotAllowed)),
        (shop::Status::Suspended, false, Err(Denied::ShopSuspended)),
        (shop::Status::Closed, false, Err(Denied::NotAllowed)),
    ];
    for (status, is_seller, manage) in cases {
        set_shop_status(&db, market.shop.id, status).await;
        assert_eq!(roles_of(&db, seller).await.seller, is_seller, "{status:?}");
        assert_eq!(
            check(&db, seller, Action::ManageShop).await,
            manage,
            "{status:?}"
        );
    }

    // A buyer without a shop has no seller role and nothing to manage
    assert_eq!(roles_of(&db, market.buyer.id).await, Roles::default());
    assert_eq!(
        check(&db, market.buyer.id, Action::ManageShop).await,
        Err(Denied::NotAllowed)
    );
}

/// NFR-SEC-02: a product is managed only by its own shop.
#[tokio::test]
async fn products_are_managed_only_by_their_shop() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let other_seller = new_user("U-other-seller").insert(&db).await.unwrap();
    new_shop(other_seller.id).insert(&db).await.unwrap();
    let product = Action::ManageProduct(market.product.id);

    assert_eq!(check(&db, market.shop.owner_id, product).await, Ok(()));
    assert_eq!(
        check(&db, other_seller.id, product).await,
        Err(Denied::NotAllowed)
    );
    assert_eq!(
        check(&db, market.buyer.id, product).await,
        Err(Denied::NotAllowed)
    );
    assert_eq!(
        check(
            &db,
            market.shop.owner_id,
            Action::ManageProduct(market.product.id + 1)
        )
        .await,
        Err(Denied::NotFound)
    );
}

/// FR-ADM-01: only admins administer, and not while suspended.
#[tokio::test]
async fn only_admins_administer() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let order = new_pending_order(&db, &market).await;
    let admin = new_admin(&db, "U-admin").await;

    assert_eq!(check(&db, admin.id, Action::Administer).await, Ok(()));
    for user_id in [market.buyer.id, market.shop.owner_id] {
        assert_eq!(
            check(&db, user_id, Action::Administer).await,
            Err(Denied::NotAllowed)
        );
    }

    set_user_status(&db, admin.id, user::Status::Suspended).await;
    assert_eq!(
        check(&db, admin.id, Action::Administer).await,
        Err(Denied::Suspended)
    );
    // A suspended admin sees only their own orders
    assert_eq!(
        check(&db, admin.id, Action::ViewOrder(order.id)).await,
        Err(Denied::NotAllowed)
    );
}

#[tokio::test]
async fn deleted_and_unknown_users_cannot_act() {
    let (_container, db) = database().await;
    let user = new_user("U-gone").insert(&db).await.unwrap();
    set_user_status(&db, user.id, user::Status::Deleted).await;

    assert_eq!(Actor::load(&db, user.id).await.unwrap(), None);
    assert_eq!(Actor::load(&db, user.id + 1).await.unwrap(), None);
}

/// FR-ADM-01: a listed user who follows after startup gets the admin role then.
#[tokio::test]
async fn a_listed_user_is_an_admin_from_their_first_event() {
    let (_container, db) = database().await;
    let admins = AdminConfig {
        line_user_ids: vec!["U-listed".to_owned()],
    };

    let listed = roles::register(&db, &admins, "U-listed", at())
        .await
        .unwrap();
    let buyer = roles::register(&db, &admins, "U-buyer", at())
        .await
        .unwrap();
    assert!(listed.is_admin);
    assert!(!buyer.is_admin);
    for user in [&listed, &buyer] {
        assert!(user.is_following);
        assert_eq!(user.status, user::Status::Active);
        assert_eq!(user.menu_roles, None);
        assert_eq!(user.created_at, at());
    }
    assert_eq!(listed.line_user_id.as_deref(), Some("U-listed"));

    // Later events find the same user, unchanged
    let later = at() + TimeDelta::hours(1);
    let again = roles::register(&db, &admins, "U-listed", later)
        .await
        .unwrap();
    assert_eq!(again, listed);

    // FR-ADM-08: the grant is audited once, as the system's
    let grant = (
        None,
        "admin.granted".to_owned(),
        Some(listed.id),
        json!({ "reason": "listed in admin.lineUserIds" }),
    );
    assert_eq!(audited(&db).await, [grant]);
}

/// FR-UI-10: whatever changes a user's roles, their menu is outdated until relinked.
#[tokio::test]
async fn menus_are_outdated_until_relinked() {
    let (_container, db) = database().await;
    let market = market(&db).await;
    let seller = market.shop.owner_id;
    let seller_menu = Outdated {
        user_id: seller,
        line_user_id: "U-seller".to_owned(),
        menu: Some(MenuRoles::Seller),
    };

    // A buyer on the default menu is up to date; the approved seller isn't yet
    assert_eq!(outdated(&db, None).await, slice::from_ref(&seller_menu));
    assert_eq!(outdated(&db, Some(market.buyer.id)).await, []);
    roles::menu_linked(&db, seller, Some(MenuRoles::Seller))
        .await
        .unwrap();
    assert_eq!(outdated(&db, None).await, []);

    // The admin sync grants the buyer the admin role
    let txn = db.begin().await.unwrap();
    let admins = AdminConfig {
        line_user_ids: vec!["U-buyer".to_owned()],
    };
    roles::sync_admins(&txn, &admins, at()).await.unwrap();
    txn.commit().await.unwrap();
    let buyer_menu = Outdated {
        user_id: market.buyer.id,
        line_user_id: "U-buyer".to_owned(),
        menu: Some(MenuRoles::Admin),
    };
    assert_eq!(outdated(&db, None).await, [buyer_menu]);
    assert_eq!(outdated(&db, Some(seller)).await, []);

    // Suspending the shop takes the seller role, and its tab, away
    set_shop_status(&db, market.shop.id, shop::Status::Suspended).await;
    let seller_default = Outdated {
        menu: None,
        ..seller_menu
    };
    assert_eq!(outdated(&db, Some(seller)).await, [seller_default]);

    // A user who unfollowed can't be linked a menu until they follow again
    let mut buyer = user::Entity::find_by_id(market.buyer.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap()
        .into_active_model();
    buyer.is_following = Set(false);
    buyer.update(&db).await.unwrap();
    assert_eq!(outdated(&db, Some(market.buyer.id)).await, []);
}
