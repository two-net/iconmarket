//! Rich menus (§3.1.2): one per role, linked to each user for the roles they hold, with tabs to
//! switch between those roles (FR-UI-10), created or updated at startup (FR-UI-11).
//!
//! A user who is only a buyer has the default menu, of the buyer's functions. A user with more
//! roles is linked to the buyer tab of the menus for their roles: one menu per tab, each with a
//! bar along the top of only the tabs for those roles. Tapping a tab switches to its menu
//! through an alias, which LINE does on the phone; the postback it sends needs no answer
//! ([`SWITCH_TAB`](crate::dispatcher::menu::SWITCH_TAB)). Every function is the postback of
//! one of the main menu's [`Item`](crate::dispatcher::menu::Item)s, so it works the same as from
//! the `เมนู` message.
//!
//! The images ship with the application, in `assets/rich-menus/`. `render.py` draws them from
//! `menus.json`, which a test checks against the menus defined here: a label or tap area changed
//! here means updating `menus.json` and drawing the images again.
//!
//! Each menu's name at LINE ends in a fingerprint of its tap areas and image, so a start reuses
//! the menus that are unchanged and creates only those that changed. The users linked to a
//! replaced menu are relinked to the new one, and the old one is deleted.

#[cfg(test)]
mod tests;

use anyhow::Context;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Iterable, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::templates::{item_label, labels, tab_title};
use crate::dispatcher::menu::Tab;
use crate::line::{self, Action, Area, Bounds, ImageType, RichMenu, RichMenuInfo, Size};
use crate::persistence::entity::user::MenuRoles;
use crate::users::roles::{self, Roles};

/// Of every menu's image: the largest LINE takes, so labels can be large
const SIZE: Size = Size {
    width: 2500,
    height: 1686,
};
/// Of the bar of tabs along the top of a menu with tabs
const TAB_BAR_HEIGHT: u32 = 236;
/// The six functions are laid out 3 across and 2 down
const COLUMNS: u32 = 3;
const ROWS: u32 = 2;

/// Starts the name of each of IconMarket's menus at LINE; menus named otherwise are left alone
const NAME_PREFIX: &str = "iconmarket/";
/// Likewise for aliases
const ALIAS_PREFIX: &str = "im-";

/// The class of the advisory lock held while provisioning, a key space of its own
const LOCK_CLASS: i32 = 0x1b0d;

/// Each menu's image, by key, drawn by `assets/rich-menus/render.py`
const IMAGES: [(&str, &[u8]); 8] = [
    ("buyer", include_bytes!("../../assets/rich-menus/buyer.png")),
    (
        "seller.buyer",
        include_bytes!("../../assets/rich-menus/seller.buyer.png"),
    ),
    (
        "seller.seller",
        include_bytes!("../../assets/rich-menus/seller.seller.png"),
    ),
    (
        "admin.buyer",
        include_bytes!("../../assets/rich-menus/admin.buyer.png"),
    ),
    (
        "admin.admin",
        include_bytes!("../../assets/rich-menus/admin.admin.png"),
    ),
    (
        "seller_admin.buyer",
        include_bytes!("../../assets/rich-menus/seller_admin.buyer.png"),
    ),
    (
        "seller_admin.seller",
        include_bytes!("../../assets/rich-menus/seller_admin.seller.png"),
    ),
    (
        "seller_admin.admin",
        include_bytes!("../../assets/rich-menus/seller_admin.admin.png"),
    ),
];

/// IconMarket's menus at LINE, as relinking needs them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichMenus {
    /// The ID of the menu linked to the users with each set of roles beyond buyer: its buyer
    /// tab. Everyone else has the default menu.
    linked: Vec<(MenuRoles, String)>,
}

impl RichMenus {
    /// The menus `ids` are the IDs of.
    fn new(ids: &[(Menu, String)]) -> Self {
        let linked = ids
            .iter()
            .filter(|(menu, _)| menu.tab == Tab::Buyer)
            .filter_map(|(menu, id)| Some((menu.roles?, id.clone())))
            .collect();
        Self { linked }
    }

    #[cfg(test)]
    pub fn for_test(linked: Vec<(MenuRoles, String)>) -> Self {
        Self { linked }
    }

    /// The ID of the menu to link to users with `roles`.
    fn linked(&self, roles: MenuRoles) -> &str {
        self.linked
            .iter()
            .find(|(of, _)| *of == roles)
            .map(|(_, id)| id.as_str())
            .expect("a menu for every set of roles")
    }
}

/// What [`relink_rich_menus`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Relinked {
    pub linked: usize,
    /// Those left outdated, to try again
    pub failed: usize,
}

/// One of the menus: a tab of the menus for a set of roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Menu {
    /// The roles beyond buyer it is for: `None` for the default menu
    roles: Option<MenuRoles>,
    /// The tab it shows
    tab: Tab,
}

impl Menu {
    /// Every menu: the default one, then each set of roles' tabs in order.
    fn all() -> impl Iterator<Item = Self> {
        let default = Self {
            roles: None,
            tab: Tab::Buyer,
        };
        let tabbed = MenuRoles::iter().flat_map(|roles| {
            Tab::of(Roles::of_menu(Some(roles))).map(move |tab| Self {
                roles: Some(roles),
                tab,
            })
        });
        [default].into_iter().chain(tabbed)
    }

    /// The tabs on its bar; the default menu has no bar.
    fn tabs(self) -> Vec<Tab> {
        match self.roles {
            Some(roles) => Tab::of(Roles::of_menu(Some(roles))).collect(),
            None => Vec::new(),
        }
    }

    /// Where its functions start, below its bar of tabs if it has one
    fn top(self) -> u32 {
        if self.roles.is_some() {
            TAB_BAR_HEIGHT
        } else {
            0
        }
    }

    /// `buyer` for the default menu, else its roles' and tab's, as `seller_admin.seller`
    fn key(self) -> String {
        match self.roles {
            Some(roles) => format!("{}.{}", roles_key(roles), self.tab.key()),
            None => self.tab.key().to_owned(),
        }
    }

    /// The alias its tab switches to; nothing switches to the default menu
    fn alias(self) -> Option<String> {
        let roles = self.roles?;
        Some(format!(
            "{ALIAS_PREFIX}{}-{}",
            roles_key(roles),
            self.tab.key()
        ))
    }

    fn image(self) -> &'static [u8] {
        let key = self.key();
        IMAGES
            .iter()
            .find(|(of, _)| *of == key)
            .map(|(_, image)| *image)
            .expect("an image for every menu")
    }

    /// The menu as LINE takes it, named with its fingerprint. A function is tapped where its
    /// cell is, and every tab but its own switches to that tab's menu.
    fn rich_menu(self) -> RichMenu {
        let seller = Roles::of_menu(self.roles).seller;
        let cells = (0..).zip(self.tab.items()).map(|(index, item)| Area {
            bounds: cell_bounds(index, self.top()),
            action: item.postback().button(item_label(item, seller).clone()),
        });
        let tabs = self.tabs();
        let count = tabs.len();
        let switches = (0..)
            .zip(tabs)
            .filter(|&(_, tab)| tab != self.tab)
            .map(|(index, tab)| Area {
                bounds: tab_bounds(index, count),
                action: Action::RichMenuSwitch {
                    label: tab_title(tab).clone(),
                    rich_menu_alias_id: Self { tab, ..self }.alias().expect("a tab's alias"),
                    data: tab.switch_postback().to_string(),
                },
            });
        let mut menu = RichMenu {
            size: SIZE,
            selected: true,
            name: self.key(),
            chat_bar_text: labels::MENU.as_str().to_owned(),
            areas: cells.chain(switches).collect(),
        };
        menu.name = format!(
            "{NAME_PREFIX}{}/{}",
            self.key(),
            fingerprint(&menu, self.image())
        );
        menu
    }
}

/// The name of `roles` in the names of their menus and aliases.
fn roles_key(roles: MenuRoles) -> &'static str {
    match roles {
        MenuRoles::Seller => "seller",
        MenuRoles::Admin => "admin",
        MenuRoles::SellerAdmin => "seller_admin",
    }
}

/// The cell of function `index`, left to right then top to bottom, below `top`.
fn cell_bounds(index: u32, top: u32) -> Bounds {
    let (column, row) = (index % COLUMNS, index / COLUMNS);
    let x = |column| column * SIZE.width / COLUMNS;
    let y = |row| top + row * (SIZE.height - top) / ROWS;
    Bounds {
        x: x(column),
        y: y(row),
        width: x(column + 1) - x(column),
        height: y(row + 1) - y(row),
    }
}

/// Tab `index` of `count` on the bar.
fn tab_bounds(index: u32, count: usize) -> Bounds {
    let count = u32::try_from(count).expect("a few tabs");
    let x = |index| index * SIZE.width / count;
    Bounds {
        x: x(index),
        y: 0,
        width: x(index + 1) - x(index),
        height: TAB_BAR_HEIGHT,
    }
}

/// The first 16 hex digits of the SHA-256 of `menu`, as sent, and `image`.
fn fingerprint(menu: &RichMenu, image: &[u8]) -> String {
    let menu = serde_json::to_vec(menu).expect("a menu serializes");
    let digest = Sha256::new()
        .chain_update(menu)
        .chain_update(image)
        .finalize();
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The menus at LINE (FR-UI-11). With `auto_provision`, those that are missing or changed are
/// created, the aliases and default menu set, and the replaced ones deleted; otherwise those
/// already there are used as they are. Holds a database lock meanwhile, so that instances
/// starting together don't each create them.
pub async fn provision_rich_menus(
    db: &DatabaseConnection,
    line: &line::Client,
    auto_provision: bool,
) -> anyhow::Result<RichMenus> {
    let lock = db.begin().await?;
    let take = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1, 0)",
        [LOCK_CLASS.into()],
    );
    lock.execute_raw(take).await?;
    let menus = if auto_provision {
        provision(db, line).await
    } else {
        find(line).await
    };
    // Releases the lock
    lock.rollback().await?;
    menus
}

async fn provision(db: &DatabaseConnection, line: &line::Client) -> anyhow::Result<RichMenus> {
    let wanted: Vec<(Menu, RichMenu)> = Menu::all().map(|menu| (menu, menu.rich_menu())).collect();
    let existing = ours(line).await?;

    let mut ids = Vec::new();
    let mut created = 0;
    for (menu, rich_menu) in &wanted {
        let id = match existing.iter().find(|at| at.name == rich_menu.name) {
            Some(at) => at.rich_menu_id.clone(),
            None => {
                created += 1;
                create(line, *menu, rich_menu).await?
            }
        };
        ids.push((*menu, id));
    }

    let aliases = line
        .rich_menu_aliases()
        .await
        .context("cannot list the rich-menu aliases")?;
    for (menu, id) in &ids {
        let Some(alias) = menu.alias() else {
            continue;
        };
        let pointed = match aliases.iter().find(|at| at.rich_menu_alias_id == alias) {
            None => line.create_rich_menu_alias(&alias, id).await,
            Some(at) if at.rich_menu_id != *id => line.update_rich_menu_alias(&alias, id).await,
            Some(_) => continue,
        };
        pointed.with_context(|| format!("cannot point rich-menu alias {alias} at its menu"))?;
    }
    let (_, default) = ids
        .iter()
        .find(|(menu, _)| menu.roles.is_none())
        .expect("a default menu");
    line.set_default_rich_menu(default)
        .await
        .context("cannot set the default rich menu")?;

    // The users of a replaced menu are outdated first, so that they are relinked even if the
    // app stops before the old menu is deleted
    let replaced: Vec<&RichMenuInfo> = existing
        .iter()
        .filter(|at| wanted.iter().all(|(_, menu)| menu.name != at.name))
        .collect();
    for roles in MenuRoles::iter() {
        let prefix = format!("{NAME_PREFIX}{}.", roles_key(roles));
        if replaced.iter().any(|at| at.name.starts_with(&prefix)) {
            let users = roles::menus_replaced(db, roles).await?;
            debug!(
                ?roles,
                users, "rich menus replaced; their users are relinked"
            );
        }
    }
    let wanted_aliases: Vec<String> = ids.iter().filter_map(|(menu, _)| menu.alias()).collect();
    let unwanted = aliases
        .iter()
        .map(|at| &at.rich_menu_alias_id)
        .filter(|alias| alias.starts_with(ALIAS_PREFIX) && !wanted_aliases.contains(alias));
    for alias in unwanted {
        if let Err(e) = line.delete_rich_menu_alias(alias).await {
            warn!(
                alias,
                error = %e,
                "cannot delete an old rich-menu alias; trying again at the next start"
            );
        }
    }
    let mut deleted = 0;
    for old in replaced {
        match line.delete_rich_menu(&old.rich_menu_id).await {
            Ok(()) => deleted += 1,
            Err(e) => warn!(
                name = old.name,
                error = %e,
                "cannot delete an old rich menu; trying again at the next start"
            ),
        }
    }
    info!(
        created,
        unchanged = wanted.len() - created,
        deleted,
        "rich menus provisioned"
    );
    Ok(RichMenus::new(&ids))
}

/// The menus already at LINE, whatever their fingerprint, for when provisioning is off.
async fn find(line: &line::Client) -> anyhow::Result<RichMenus> {
    let existing = ours(line).await?;
    let ids = Menu::all()
        .map(|menu| {
            let prefix = format!("{NAME_PREFIX}{}/", menu.key());
            let at = existing.iter().find(|at| at.name.starts_with(&prefix));
            at.map(|at| (menu, at.rich_menu_id.clone()))
                .with_context(|| {
                    format!(
                        "no rich menu {} at LINE, and line.richMenu.autoProvision is off",
                        menu.key()
                    )
                })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    info!(menus = ids.len(), "rich menus found at LINE");
    Ok(RichMenus::new(&ids))
}

/// IconMarket's menus at LINE.
async fn ours(line: &line::Client) -> anyhow::Result<Vec<RichMenuInfo>> {
    let menus = line
        .rich_menus()
        .await
        .context("cannot list the rich menus")?;
    Ok(menus
        .into_iter()
        .filter(|at| at.name.starts_with(NAME_PREFIX))
        .collect())
}

/// Creates `rich_menu`, which is `menu`, with its image, and returns its ID.
async fn create(line: &line::Client, menu: Menu, rich_menu: &RichMenu) -> anyhow::Result<String> {
    let name = &rich_menu.name;
    let id = line
        .create_rich_menu(rich_menu)
        .await
        .with_context(|| format!("cannot create rich menu {name}"))?;
    let image = menu.image().to_vec();
    if let Err(e) = line
        .upload_rich_menu_image(&id, ImageType::Png, image)
        .await
    {
        // Nothing is linked to it yet, and without its image nothing can be
        if let Err(e) = line.delete_rich_menu(&id).await {
            warn!(name, id, error = %e, "cannot delete a rich menu left without its image; delete it at LINE");
        }
        return Err(e).with_context(|| format!("cannot upload the image of rich menu {name}"));
    }
    info!(name, id, "rich menu created");
    Ok(id)
}

/// Links the users whose roles changed to the rich menu for their roles (FR-UI-10): all of
/// them, or only the user `only`. A user stays outdated until relinked, so a link that fails is
/// made up for by the next run. Called at startup, and after a change of roles is committed.
pub async fn relink_rich_menus(
    db: &DatabaseConnection,
    line: &line::Client,
    menus: &RichMenus,
    only: Option<i64>,
) -> Result<Relinked, DbErr> {
    let mut relinked = Relinked::default();
    for user in roles::outdated_menus(db, only).await? {
        let result = match user.menu {
            Some(roles) => {
                line.link_rich_menu(&user.line_user_id, menus.linked(roles))
                    .await
            }
            // Back to the default menu
            None => line.unlink_rich_menu(&user.line_user_id).await,
        };
        match result {
            Ok(()) => {
                roles::menu_linked(db, user.user_id, user.menu).await?;
                relinked.linked += 1;
            }
            Err(e) => {
                warn!(user_id = user.user_id, error = %e, "cannot link the user's rich menu");
                relinked.failed += 1;
            }
        }
    }
    if relinked != Relinked::default() {
        info!(
            linked = relinked.linked,
            failed = relinked.failed,
            "rich menus relinked"
        );
    }
    Ok(relinked)
}
