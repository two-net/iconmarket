//! "ซื้อของ" (FR-SRCH-01): the active categories, in the order admins set (FR-ADM-05), as
//! Quick Replies. Browsing (#28) adds "ของใหม่วันนี้" and "ร้านค้าทั้งหมด" beside them, and
//! the products a category's Quick Reply lists.

use sea_orm::DbErr;

use super::categories;
use crate::dispatcher::Turn;
use crate::dispatcher::menu::{self, Item};
use crate::dispatcher::postback::Postback;
use crate::line::{Message, limits};
use crate::notifications::templates;
use crate::persistence::entity::category;
use crate::thai::Label;
use crate::users::access::{self, authorize};

/// The action of a category's Quick Reply; its `category` is the category's ID
pub const CATEGORY: &str = "browse";
const CATEGORY_ID: &str = "category";
/// The page of categories "ดูเพิ่มเติม" shows, on the postback of "ซื้อของ"
const PAGE: &str = "page";
/// Categories on a page of Quick Replies, leaving room for "ดูเพิ่มเติม"
const CATEGORIES_PER_PAGE: usize = limits::QUICK_REPLY_ITEMS - 1;

/// A tap on "ซื้อของ", or on "ดูเพิ่มเติม" under its categories.
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Browse).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let active = categories::active(turn.db).await?;
    if active.is_empty() {
        let none = turn.templates.nothing_to_browse();
        return Ok(vec![none, menu::main_menu(&turn.actor)]);
    }
    // A page past the last, from a button sent before categories were deactivated, starts over
    let pages = active.len().div_ceil(CATEGORIES_PER_PAGE);
    let page = postback
        .int(PAGE)
        .and_then(|page| usize::try_from(page).ok())
        .filter(|&page| page < pages)
        .unwrap_or(0);
    let choice = |category: &category::Model| {
        let browse = Postback::new(CATEGORY).with_int(CATEGORY_ID, category.id);
        browse.button(Label::fit(category.name.as_str()))
    };
    let more = |next| {
        let more = Item::Buy.postback().with_int(PAGE, next as i64);
        more.to_string()
    };
    let choices = templates::quick_reply_page(&active, page, CATEGORIES_PER_PAGE, choice, more);
    Ok(vec![turn.templates.choose_category(choices)])
}
