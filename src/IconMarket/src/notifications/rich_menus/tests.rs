//! The rich menus (§3.1.2): their tabs and tap areas, the images drawn for them, provisioning
//! against a fake LINE (FR-UI-11) and relinking as roles change (FR-UI-10). The `async` tests
//! each start a throwaway PostgreSQL, so they need Docker.

use std::collections::HashSet;

use axum::http::Method;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, IntoActiveModel, TransactionTrait,
};
use serde_json::{Value, json};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::AdminConfig;
use crate::dispatcher::menu::{Item, SWITCH_TAB};
use crate::dispatcher::postback::Postback;
use crate::line::limits::{self, RICH_MENU_IMAGE_BYTES};
use crate::line::testing::{Answer, Fake, Request};
use crate::persistence::entity::{shop, user};
use crate::persistence::orm;
use crate::persistence::testing::{self, at, market};

const LIST: &str = "/v2/bot/richmenu/list";
const CREATE: &str = "/v2/bot/richmenu";
const ALIASES: &str = "/v2/bot/richmenu/alias/list";
const ALIAS: &str = "/v2/bot/richmenu/alias";

/// The drawn text of `item`'s cell: its label, but for the default menu's shop, which is both
/// opening a shop and the shop of a user who has one that isn't approved yet.
fn lines(item: Item, seller: bool) -> Vec<String> {
    match item {
        Item::MyShop if !seller => vec![
            format!("{} /", labels::OPEN_SHOP.as_str()),
            labels::MY_SHOP.as_str().to_owned(),
        ],
        _ => vec![item_label(item, seller).as_str().to_owned()],
    }
}

/// What `render.py` draws, as `menus.json` has it: each menu's bar of tabs, if it has one, and
/// the cells of its functions.
fn spec() -> Value {
    let menus: Vec<Value> = Menu::all()
        .map(|menu| {
            let tabs = menu.tabs();
            let bar: Vec<Value> = (0..)
                .zip(&tabs)
                .map(|(index, &tab)| {
                    json!({
                        "tab": tab.key(),
                        "label": tab_title(tab).as_str(),
                        "active": tab == menu.tab,
                        "bounds": tab_bounds(index, tabs.len()),
                    })
                })
                .collect();
            let seller = Roles::of_menu(menu.roles).seller;
            let cells: Vec<Value> = (0..)
                .zip(menu.tab.items())
                .map(|(index, item)| {
                    json!({
                        "action": item.action(),
                        "lines": lines(item, seller),
                        "bounds": cell_bounds(index, menu.top()),
                    })
                })
                .collect();
            json!({
                "image": format!("{}.png", menu.key()),
                "tab": menu.tab.key(),
                "tabs": bar,
                "cells": cells,
            })
        })
        .collect();
    json!({ "size": SIZE, "menus": menus })
}

/// FR-UI-11: the images shipped are drawn from the menus defined here.
#[test]
fn the_images_are_drawn_from_these_menus() {
    let drawn: Value =
        serde_json::from_str(include_str!("../../../assets/rich-menus/menus.json")).unwrap();
    assert!(
        drawn == spec(),
        "assets/rich-menus/menus.json is out of date: replace it with this, then run \
         render.py next to it\n{}",
        serde_json::to_string_pretty(&spec()).unwrap()
    );
    for (key, png) in IMAGES {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
        assert_eq!(
            (image.width(), image.height()),
            (SIZE.width, SIZE.height),
            "{key}"
        );
        assert!(
            png.len() <= RICH_MENU_IMAGE_BYTES,
            "{key}: {} bytes",
            png.len()
        );
    }
}

#[test]
fn every_menu_is_one_line_accepts() {
    let menus: Vec<_> = Menu::all().collect();
    assert_eq!(menus.len(), IMAGES.len());
    let mut names = HashSet::new();
    for menu in menus {
        let rich_menu = menu.rich_menu();
        limits::check_rich_menu(&rich_menu).unwrap();
        assert!(names.insert(rich_menu.name.clone()), "{}", rich_menu.name);
        // Every pixel is tapped somewhere, and in one place only
        let area = |bounds: Bounds| u64::from(bounds.width) * u64::from(bounds.height);
        let tabs = menu.tabs();
        let bar: u64 = (0..)
            .zip(&tabs)
            .map(|(i, _)| area(tab_bounds(i, tabs.len())))
            .sum();
        let cells: u64 = (0..6).map(|i| area(cell_bounds(i, menu.top()))).sum();
        assert_eq!(
            bar + cells,
            area(Bounds {
                x: 0,
                y: 0,
                width: SIZE.width,
                height: SIZE.height
            })
        );
    }
}

/// FR-UI-10: each menu has its tab's functions, and switches to the other tabs of its roles.
#[test]
fn each_menu_has_its_functions_and_the_tabs_for_its_roles() {
    let aliases: Vec<String> = Menu::all().filter_map(Menu::alias).collect();
    assert_eq!(aliases.len(), 7);
    assert_eq!(aliases.iter().collect::<HashSet<_>>().len(), aliases.len());
    for alias in &aliases {
        assert!(alias.len() <= 32, "{alias}");
        assert!(
            alias
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b)),
            "{alias}"
        );
    }

    for menu in Menu::all() {
        let mut items = Vec::new();
        let mut switches = Vec::new();
        for area in menu.rich_menu().areas {
            match area.action {
                Action::Postback { data, .. } => {
                    let postback = Postback::parse(&data).unwrap();
                    items.push(Item::from_action(postback.action()).unwrap());
                }
                Action::RichMenuSwitch {
                    rich_menu_alias_id,
                    data,
                    ..
                } => {
                    let postback = Postback::parse(&data).unwrap();
                    assert_eq!(postback.action(), SWITCH_TAB);
                    switches.push((postback.name("to").unwrap().to_owned(), rich_menu_alias_id));
                }
                action => panic!("{action:?}"),
            }
        }
        assert_eq!(items, menu.tab.items(), "{}", menu.key());
        let others: Vec<_> = menu
            .tabs()
            .into_iter()
            .filter(|&tab| tab != menu.tab)
            .map(|tab| {
                let alias = Menu { tab, ..menu }.alias().unwrap();
                (tab.key().to_owned(), alias)
            })
            .collect();
        assert_eq!(switches, others, "{}", menu.key());
    }

    let tabs = |roles| {
        Menu {
            roles,
            tab: Tab::Buyer,
        }
        .tabs()
    };
    assert_eq!(tabs(None), []);
    assert_eq!(tabs(Some(MenuRoles::Seller)), [Tab::Buyer, Tab::Seller]);
    assert_eq!(tabs(Some(MenuRoles::Admin)), [Tab::Buyer, Tab::Admin]);
    assert_eq!(
        tabs(Some(MenuRoles::SellerAdmin)),
        [Tab::Buyer, Tab::Seller, Tab::Admin]
    );
}

#[test]
fn a_menu_changed_in_any_way_gets_another_name() {
    let menu = Menu::all().next().unwrap().rich_menu();
    assert_eq!(fingerprint(&menu, b"png"), fingerprint(&menu, b"png"));
    assert_ne!(fingerprint(&menu, b"png"), fingerprint(&menu, b"PNG"));
    let fewer = RichMenu {
        areas: menu.areas[1..].to_vec(),
        ..menu.clone()
    };
    assert_ne!(fingerprint(&menu, b"png"), fingerprint(&fewer, b"png"));
}

async fn database() -> (ContainerAsync<Postgres>, DatabaseConnection) {
    let (container, pool) = testing::migrated().await;
    (container, orm(pool))
}

fn rich_menus() -> RichMenus {
    RichMenus::for_test(vec![
        (MenuRoles::Seller, "richmenu-seller".to_owned()),
        (MenuRoles::Admin, "richmenu-admin".to_owned()),
        (MenuRoles::SellerAdmin, "richmenu-both".to_owned()),
    ])
}

/// LINE listing `menus`, with the IDs `richmenu-1`, `richmenu-2` and so on.
fn listed<'a>(menus: impl IntoIterator<Item = &'a str>) -> Answer {
    let menus: Vec<Value> = (1..)
        .zip(menus)
        .map(|(i, name)| json!({ "richMenuId": format!("richmenu-{i}"), "name": name }))
        .collect();
    Answer::json(200, json!({ "richmenus": menus }))
}

/// LINE listing `aliases`, each with the ID of its menu.
fn aliased<'a>(aliases: impl IntoIterator<Item = (&'a str, &'a str)>) -> Answer {
    let aliases: Vec<Value> = aliases
        .into_iter()
        .map(|(alias, id)| json!({ "richMenuAliasId": alias, "richMenuId": id }))
        .collect();
    Answer::json(200, json!({ "aliases": aliases }))
}

/// The names of the menus as wanted now, in order.
fn names() -> Vec<String> {
    Menu::all().map(|menu| menu.rich_menu().name).collect()
}

/// Each wanted menu's alias, with the ID it has when LINE lists them all in order.
fn aliases() -> Vec<(String, String)> {
    (1..)
        .zip(Menu::all())
        .filter_map(|(i, menu)| Some((menu.alias()?, format!("richmenu-{i}"))))
        .collect()
}

/// The method and path of each request, from the `from`th on.
fn calls(fake: &Fake, from: usize) -> Vec<(Method, String)> {
    fake.requests()[from..]
        .iter()
        .map(|request| (request.method.clone(), request.path.clone()))
        .collect()
}

fn post(path: impl Into<String>) -> (Method, String) {
    (Method::POST, path.into())
}

fn get(path: &str) -> (Method, String) {
    (Method::GET, path.to_owned())
}

fn delete(path: impl Into<String>) -> (Method, String) {
    (Method::DELETE, path.into())
}

/// AC: restarting with unchanged menus does not create duplicates (FR-UI-11).
#[tokio::test]
async fn menus_are_created_once_and_reused_after_a_restart() {
    let (_container, db) = database().await;
    let fake = Fake::start().await;
    let line = fake.client();

    // The first start: only a menu someone else made
    fake.script(LIST, [listed(["campaign"])]);
    fake.script(ALIASES, [aliased([("campaign", "richmenu-1")])]);
    let ids: Vec<String> = (1..=8).map(|i| format!("richmenu-{i}")).collect();
    fake.script(
        CREATE,
        ids.iter()
            .map(|id| Answer::json(200, json!({ "richMenuId": id }))),
    );
    let menus = provision_rich_menus(&db, &line, true).await.unwrap();

    let requests = fake.requests();
    let created: Vec<&Request> = requests.iter().filter(|r| r.path == CREATE).collect();
    let created: Vec<String> = created
        .iter()
        .map(|request| request.json()["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(created, names());
    let uploads: Vec<_> = requests
        .iter()
        .filter(|request| request.host == "data")
        .map(|request| (request.path.clone(), request.header("content-type")))
        .collect();
    let expected: Vec<_> = ids
        .iter()
        .map(|id| (format!("/v2/bot/richmenu/{id}/content"), Some("image/png")))
        .collect();
    assert_eq!(uploads, expected);
    let pointed: Vec<_> = requests
        .iter()
        .filter(|request| request.path == ALIAS)
        .map(|request| {
            let body = request.json();
            let alias = body["richMenuAliasId"].as_str().unwrap().to_owned();
            (alias, body["richMenuId"].as_str().unwrap().to_owned())
        })
        .collect();
    assert_eq!(pointed, aliases());
    let rest: Vec<_> = calls(&fake, 0)
        .into_iter()
        .filter(|(_, path)| path != CREATE && path != ALIAS && !path.ends_with("/content"))
        .collect();
    assert_eq!(
        rest,
        [
            get(LIST),
            get(ALIASES),
            post("/v2/bot/user/all/richmenu/richmenu-1")
        ]
    );
    assert_eq!(
        menus,
        RichMenus::for_test(vec![
            (MenuRoles::Seller, "richmenu-2".to_owned()),
            (MenuRoles::Admin, "richmenu-4".to_owned()),
            (MenuRoles::SellerAdmin, "richmenu-6".to_owned()),
        ])
    );

    // A restart finds them all as they were made
    let from = fake.requests().len();
    let names = names();
    let aliases = aliases();
    fake.script(LIST, [listed(names.iter().map(String::as_str))]);
    fake.script(
        ALIASES,
        [aliased(
            aliases.iter().map(|(a, id)| (a.as_str(), id.as_str())),
        )],
    );
    let again = provision_rich_menus(&db, &line, true).await.unwrap();
    assert_eq!(
        calls(&fake, from),
        [
            get(LIST),
            get(ALIASES),
            post("/v2/bot/user/all/richmenu/richmenu-1")
        ]
    );
    assert_eq!(again, menus);
}

/// FR-UI-11: a menu that changed is created anew, its alias moved to it, its users relinked
/// and the old one deleted.
#[tokio::test]
async fn a_changed_menu_replaces_the_old_one() {
    let (_container, db) = database().await;
    let fake = Fake::start().await;
    let line = fake.client();
    let on_seller_menu = user::ActiveModel {
        menu_roles: Set(Some(MenuRoles::Seller)),
        ..testing::new_user("U-seller")
    }
    .insert(&db)
    .await
    .unwrap();
    let on_admin_menu = user::ActiveModel {
        menu_roles: Set(Some(MenuRoles::Admin)),
        ..testing::new_user("U-admin")
    }
    .insert(&db)
    .await
    .unwrap();

    // The seller's buyer tab, `richmenu-2`, was drawn differently before
    let mut names = names();
    let seller_buyer = std::mem::replace(
        &mut names[1],
        "iconmarket/seller.buyer/0123456789abcdef".to_owned(),
    );
    let aliases = aliases();
    fake.script(LIST, [listed(names.iter().map(String::as_str))]);
    fake.script(
        ALIASES,
        [aliased(
            aliases
                .iter()
                .map(|(a, id)| (a.as_str(), id.as_str()))
                .chain([("im-old", "richmenu-2")]),
        )],
    );
    fake.script(
        CREATE,
        [Answer::json(200, json!({ "richMenuId": "richmenu-9" }))],
    );
    let menus = provision_rich_menus(&db, &line, true).await.unwrap();

    let created: Vec<_> = fake
        .requests()
        .iter()
        .filter(|request| request.path == CREATE)
        .map(|request| request.json()["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(created, [seller_buyer]);
    assert_eq!(
        calls(&fake, 0),
        [
            get(LIST),
            post(CREATE),
            post("/v2/bot/richmenu/richmenu-9/content"),
            get(ALIASES),
            post("/v2/bot/richmenu/alias/im-seller-buyer"),
            post("/v2/bot/user/all/richmenu/richmenu-1"),
            delete("/v2/bot/richmenu/alias/im-old"),
            delete("/v2/bot/richmenu/richmenu-2"),
        ]
    );
    let moved = &fake.requests()[4];
    assert_eq!(moved.json(), json!({ "richMenuId": "richmenu-9" }));
    assert_eq!(menus.linked(MenuRoles::Seller), "richmenu-9");

    // The users of the old menu are relinked; the others aren't
    assert_eq!(menu_of(&db, on_seller_menu.id).await, None);
    assert_eq!(menu_of(&db, on_admin_menu.id).await, Some(MenuRoles::Admin));
}

/// A menu whose image LINE refuses is not left behind.
#[tokio::test]
async fn a_menu_without_its_image_is_deleted() {
    let (_container, db) = database().await;
    let fake = Fake::start().await;
    fake.script(LIST, [listed([])]);
    fake.script(
        CREATE,
        [Answer::json(200, json!({ "richMenuId": "richmenu-1" }))],
    );
    fake.script(
        "/v2/bot/richmenu/richmenu-1/content",
        [Answer::error(400, "Invalid image")],
    );
    let e = provision_rich_menus(&db, &fake.client(), true)
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("cannot upload the image"),
        "{e:#}"
    );
    assert_eq!(
        calls(&fake, 0),
        [
            get(LIST),
            post(CREATE),
            post("/v2/bot/richmenu/richmenu-1/content"),
            delete("/v2/bot/richmenu/richmenu-1"),
        ]
    );
}

/// With `line.richMenu.autoProvision` off, the menus at LINE are used as they are.
#[tokio::test]
async fn without_provisioning_the_menus_at_line_are_used() {
    let (_container, db) = database().await;
    let fake = Fake::start().await;
    let line = fake.client();
    let drawn_by_hand: Vec<String> = Menu::all()
        .map(|menu| format!("iconmarket/{}/by-hand", menu.key()))
        .collect();
    fake.script(LIST, [listed(drawn_by_hand.iter().map(String::as_str))]);
    let menus = provision_rich_menus(&db, &line, false).await.unwrap();
    assert_eq!(calls(&fake, 0), [get(LIST)]);
    assert_eq!(menus.linked(MenuRoles::Admin), "richmenu-4");

    // Without them, there is nothing to link
    fake.script(
        LIST,
        [listed(drawn_by_hand[..7].iter().map(String::as_str))],
    );
    let e = provision_rich_menus(&db, &line, false).await.unwrap_err();
    assert!(e.to_string().contains("seller_admin.admin"), "{e}");
}

async fn menu_of(db: &DatabaseConnection, user_id: i64) -> Option<MenuRoles> {
    let user = user::Entity::find_by_id(user_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    user.menu_roles
}

async fn sync_admins(db: &DatabaseConnection, line_user_ids: &[&str]) {
    let admins = AdminConfig {
        line_user_ids: line_user_ids.iter().map(|&id| id.to_owned()).collect(),
    };
    let txn = db.begin().await.unwrap();
    roles::sync_admins(&txn, &admins, at()).await.unwrap();
    txn.commit().await.unwrap();
}

/// AC: granting or revoking the seller or admin role changes the user's linked menu and tabs
/// (FR-UI-10).
#[tokio::test]
async fn granting_and_revoking_roles_relinks_the_menu() {
    let (_container, db) = database().await;
    let fake = Fake::start().await;
    let line = fake.client();
    let menus = rich_menus();
    let market = market(&db).await;
    let (buyer, seller) = (market.buyer.id, market.shop.owner_id);
    let relink = async |only| relink_rich_menus(&db, &line, &menus, only).await.unwrap();
    let relinked = |linked| Relinked { linked, failed: 0 };

    // The approved seller gets the seller's tabs; the buyer keeps the default menu
    assert_eq!(relink(None).await, relinked(1));
    assert_eq!(
        calls(&fake, 0),
        [post("/v2/bot/user/U-seller/richmenu/richmenu-seller")]
    );
    assert_eq!(menu_of(&db, seller).await, Some(MenuRoles::Seller));
    assert_eq!(menu_of(&db, buyer).await, None);
    // Once linked, nothing is linked again
    assert_eq!(relink(None).await, relinked(0));
    assert_eq!(fake.requests().len(), 1);

    // Granting both the admin role adds its tab
    sync_admins(&db, &["U-buyer", "U-seller"]).await;
    let from = fake.requests().len();
    assert_eq!(relink(None).await, relinked(2));
    assert_eq!(
        calls(&fake, from),
        [
            post("/v2/bot/user/U-buyer/richmenu/richmenu-admin"),
            post("/v2/bot/user/U-seller/richmenu/richmenu-both"),
        ]
    );

    // Suspending the shop takes the seller's tab away
    let mut shop = market.shop.clone().into_active_model();
    shop.status = Set(shop::Status::Suspended);
    shop.update(&db).await.unwrap();
    let from = fake.requests().len();
    assert_eq!(relink(Some(seller)).await, relinked(1));
    assert_eq!(
        calls(&fake, from),
        [post("/v2/bot/user/U-seller/richmenu/richmenu-admin")]
    );
    assert_eq!(menu_of(&db, seller).await, Some(MenuRoles::Admin));

    // Revoking the buyer's admin role leaves them the default menu
    sync_admins(&db, &["U-seller"]).await;
    let from = fake.requests().len();
    assert_eq!(relink(Some(buyer)).await, relinked(1));
    assert_eq!(
        calls(&fake, from),
        [delete("/v2/bot/user/U-buyer/richmenu")]
    );
    assert_eq!(menu_of(&db, buyer).await, None);
}

/// A link that fails leaves the user outdated, so the next run links them.
#[tokio::test]
async fn a_failed_link_is_made_up_for_by_the_next_run() {
    let (_container, db) = database().await;
    let fake = Fake::start().await;
    let line = fake.client();
    let menus = rich_menus();
    let seller = market(&db).await.shop.owner_id;
    fake.script(
        "/v2/bot/user/U-seller/richmenu/richmenu-seller",
        [Answer::error(500, "Internal server error")],
    );

    let relinked = relink_rich_menus(&db, &line, &menus, None).await.unwrap();
    assert_eq!(
        relinked,
        Relinked {
            linked: 0,
            failed: 1
        }
    );
    assert_eq!(menu_of(&db, seller).await, None);

    let relinked = relink_rich_menus(&db, &line, &menus, None).await.unwrap();
    assert_eq!(
        relinked,
        Relinked {
            linked: 1,
            failed: 0
        }
    );
    assert_eq!(menu_of(&db, seller).await, Some(MenuRoles::Seller));
}
