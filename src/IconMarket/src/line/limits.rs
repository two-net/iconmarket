//! LINE's limits on what we send (FR-LINE-11), checked before every request so LINE never
//! refuses one for its size, and paging for lists longer than one message holds.
//!
//! Values from <https://developers.line.biz/en/reference/messaging-api/>, checked on
//! 2026-10-24. LINE can change them (A-07); update them here.

use std::fmt::Display;

use serde::Serialize;

use super::error::LimitError;
use super::message::flex::{Bubble, Component, FlexBox, FlexContainer, Hero, Image};
use super::message::{Action, Body, Message, QuickReply};
use super::rich_menu::RichMenu;

pub const MESSAGES_PER_REQUEST: usize = 5;
pub const TEXT_CHARS: usize = 5000;
pub const ALT_TEXT_CHARS: usize = 1500;
pub const BUBBLES_PER_CAROUSEL: usize = 12;
/// LINE says 30 KB; counted in thousands to stay on the safe side
pub const BUBBLE_JSON_BYTES: usize = 30_000;
/// LINE says 50 KB
pub const CAROUSEL_JSON_BYTES: usize = 50_000;
pub const QUICK_REPLY_ITEMS: usize = 13;
/// A postback's `data` and `displayText`, and a message action's `text`
pub const ACTION_TEXT_CHARS: usize = 300;
pub const URI_CHARS: usize = 1000;
pub const IMAGE_URL_CHARS: usize = 2000;
pub const RICH_MENU_AREAS: usize = 20;
pub const RICH_MENU_NAME_CHARS: usize = 300;
pub const CHAT_BAR_TEXT_CHARS: usize = 14;
pub const RICH_MENU_WIDTH: std::ops::RangeInclusive<u32> = 800..=2500;
pub const RICH_MENU_MIN_HEIGHT: u32 = 250;
/// Width over height, at least
pub const RICH_MENU_MIN_ASPECT: f64 = 1.45;
/// LINE says 1 MB
pub const RICH_MENU_IMAGE_BYTES: usize = 1_000_000;
/// Users per bulk rich-menu link request
pub const USERS_PER_BULK_LINK: usize = 500;

type Checked = Result<(), LimitError>;

/// Checks the messages of one reply or push against every limit, and names the first one
/// broken.
pub fn check(messages: &[Message]) -> Checked {
    at_least_one("messages", messages.len())?;
    at_most("messages", messages.len(), MESSAGES_PER_REQUEST)?;
    for (i, message) in messages.iter().enumerate() {
        check_message(&format!("messages[{i}]"), message)?;
    }
    Ok(())
}

fn check_message(path: &str, message: &Message) -> Checked {
    match &message.body {
        Body::Text { text } => text_of(&format!("{path}.text"), text, TEXT_CHARS)?,
        Body::Image {
            original_content_url,
            preview_image_url,
        } => {
            image_url(&format!("{path}.originalContentUrl"), original_content_url)?;
            image_url(&format!("{path}.previewImageUrl"), preview_image_url)?;
        }
        Body::Flex { alt_text, contents } => {
            text_of(&format!("{path}.altText"), alt_text, ALT_TEXT_CHARS)?;
            check_container(&format!("{path}.contents"), contents)?;
        }
    }
    match &message.quick_reply {
        Some(quick_reply) => check_quick_reply(&format!("{path}.quickReply"), quick_reply),
        None => Ok(()),
    }
}

fn check_quick_reply(path: &str, quick_reply: &QuickReply) -> Checked {
    let path = format!("{path}.items");
    at_least_one(&path, quick_reply.items.len())?;
    at_most(&path, quick_reply.items.len(), QUICK_REPLY_ITEMS)?;
    for (i, item) in quick_reply.items.iter().enumerate() {
        check_action(
            &format!("{path}[{i}].action"),
            &item.action,
            Place::QuickReply,
        )?;
    }
    Ok(())
}

fn check_container(path: &str, container: &FlexContainer) -> Checked {
    match container {
        FlexContainer::Bubble(bubble) => check_bubble(path, bubble),
        FlexContainer::Carousel(carousel) => {
            let bubbles = &carousel.contents;
            let contents = format!("{path}.contents");
            at_least_one(&contents, bubbles.len())?;
            at_most(&contents, bubbles.len(), BUBBLES_PER_CAROUSEL)?;
            if bubbles.iter().any(|bubble| bubble.size != bubbles[0].size) {
                return broken(&contents, "bubbles of different sizes");
            }
            for (i, bubble) in bubbles.iter().enumerate() {
                check_bubble(&format!("{contents}[{i}]"), bubble)?;
            }
            json_size(path, carousel, CAROUSEL_JSON_BYTES)
        }
    }
}

fn check_bubble(path: &str, bubble: &Bubble) -> Checked {
    for (block, flex_box) in [
        ("header", &bubble.header),
        ("body", &bubble.body),
        ("footer", &bubble.footer),
    ] {
        if let Some(flex_box) = flex_box {
            check_box(&format!("{path}.{block}"), flex_box)?;
        }
    }
    match &bubble.hero {
        Some(Hero::Box(flex_box)) => check_box(&format!("{path}.hero"), flex_box)?,
        Some(Hero::Image(image)) => check_image(&format!("{path}.hero"), image)?,
        None => {}
    }
    optional_action(&format!("{path}.action"), bubble.action.as_ref())?;
    json_size(path, bubble, BUBBLE_JSON_BYTES)
}

fn check_box(path: &str, flex_box: &FlexBox) -> Checked {
    for (i, component) in flex_box.contents.iter().enumerate() {
        check_component(&format!("{path}.contents[{i}]"), component)?;
    }
    optional_action(&format!("{path}.action"), flex_box.action.as_ref())
}

fn check_component(path: &str, component: &Component) -> Checked {
    match component {
        Component::Box(flex_box) => check_box(path, flex_box),
        Component::Text(text) => {
            text_of(&format!("{path}.text"), &text.text, usize::MAX)?;
            optional_action(&format!("{path}.action"), text.action.as_ref())
        }
        Component::Image(image) => check_image(path, image),
        Component::Button(button) => {
            check_action(&format!("{path}.action"), &button.action, Place::Flex)
        }
        Component::Separator(_) => Ok(()),
    }
}

fn check_image(path: &str, image: &Image) -> Checked {
    image_url(&format!("{path}.url"), &image.url)?;
    optional_action(&format!("{path}.action"), image.action.as_ref())
}

fn optional_action(path: &str, action: Option<&Action>) -> Checked {
    match action {
        Some(action) => check_action(path, action, Place::Flex),
        None => Ok(()),
    }
}

/// Where an action is used; some work in only one place.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    QuickReply,
    Flex,
    RichMenu,
}

fn check_action(path: &str, action: &Action, place: Place) -> Checked {
    text_of(
        &format!("{path}.label"),
        action.label().as_str(),
        usize::MAX,
    )?;
    match action {
        Action::Postback {
            data, display_text, ..
        } => {
            text_of(&format!("{path}.data"), data, ACTION_TEXT_CHARS)?;
            match display_text {
                Some(text) => text_of(&format!("{path}.displayText"), text, ACTION_TEXT_CHARS),
                None => Ok(()),
            }
        }
        Action::Message { text, .. } => text_of(&format!("{path}.text"), text, ACTION_TEXT_CHARS),
        Action::Uri { uri, .. } => text_of(&format!("{path}.uri"), uri, URI_CHARS),
        Action::Camera { .. } | Action::CameraRoll { .. } | Action::Location { .. } => {
            only_in(path, place, Place::QuickReply, "Quick Replies")
        }
        Action::RichMenuSwitch { data, .. } => {
            only_in(path, place, Place::RichMenu, "rich menus")?;
            text_of(&format!("{path}.data"), data, ACTION_TEXT_CHARS)
        }
    }
}

fn only_in(path: &str, place: Place, allowed: Place, name: &str) -> Checked {
    if place == allowed {
        Ok(())
    } else {
        broken(path, format!("this action works only in {name}"))
    }
}

/// Checks a rich menu (§3.1.2) before it is created.
pub fn check_rich_menu(menu: &RichMenu) -> Checked {
    let (width, height) = (menu.size.width, menu.size.height);
    if !RICH_MENU_WIDTH.contains(&width) {
        return broken(
            "size.width",
            format!(
                "{width}, must be {} to {}",
                RICH_MENU_WIDTH.start(),
                RICH_MENU_WIDTH.end()
            ),
        );
    }
    if height < RICH_MENU_MIN_HEIGHT {
        return broken(
            "size.height",
            format!("{height}, at least {RICH_MENU_MIN_HEIGHT}"),
        );
    }
    if f64::from(width) / f64::from(height) < RICH_MENU_MIN_ASPECT {
        return broken(
            "size",
            format!("{width}×{height}, width over height must be at least {RICH_MENU_MIN_ASPECT}"),
        );
    }
    text_of("name", &menu.name, RICH_MENU_NAME_CHARS)?;
    text_of("chatBarText", &menu.chat_bar_text, CHAT_BAR_TEXT_CHARS)?;
    at_least_one("areas", menu.areas.len())?;
    at_most("areas", menu.areas.len(), RICH_MENU_AREAS)?;
    for (i, area) in menu.areas.iter().enumerate() {
        let path = format!("areas[{i}]");
        let bounds = area.bounds;
        let fits = bounds.width > 0
            && bounds.height > 0
            && bounds.x.saturating_add(bounds.width) <= width
            && bounds.y.saturating_add(bounds.height) <= height;
        if !fits {
            return broken(&format!("{path}.bounds"), "outside the menu");
        }
        check_action(&format!("{path}.action"), &area.action, Place::RichMenu)?;
    }
    Ok(())
}

/// One page of a list, for lists longer than one carousel or set of Quick Replies holds.
#[derive(Debug, PartialEq)]
pub struct Page<'a, T> {
    pub items: &'a [T],
    /// The next page's number, when items remain. The "ดูเพิ่มเติม" button sends it back in
    /// its postback, and the flow fetches that page.
    pub next: Option<usize>,
}

/// Page `number` (from 0) of `items`, `per_page` items to a page.
pub fn page<T>(items: &[T], number: usize, per_page: usize) -> Page<'_, T> {
    assert!(per_page > 0, "a page holds at least one item");
    let start = number.saturating_mul(per_page).min(items.len());
    let end = start.saturating_add(per_page).min(items.len());
    Page {
        items: &items[start..end],
        next: (end < items.len()).then_some(number + 1),
    }
}

fn text_of(path: &str, text: &str, max_chars: usize) -> Checked {
    if text.is_empty() {
        return broken(path, "empty");
    }
    let chars = text.chars().count();
    if chars > max_chars {
        return broken(path, format!("{chars} characters, at most {max_chars}"));
    }
    Ok(())
}

fn image_url(path: &str, url: &str) -> Checked {
    if !url.starts_with("https://") {
        return broken(path, "not an HTTPS URL");
    }
    text_of(path, url, IMAGE_URL_CHARS)
}

fn at_least_one(path: &str, count: usize) -> Checked {
    if count == 0 {
        return broken(path, "empty");
    }
    Ok(())
}

fn at_most(path: &str, count: usize, max: usize) -> Checked {
    if count > max {
        return broken(path, format!("{count} items, at most {max}"));
    }
    Ok(())
}

fn json_size(path: &str, value: &impl Serialize, max_bytes: usize) -> Checked {
    let bytes = serde_json::to_vec(value).map_or(usize::MAX, |json| json.len());
    if bytes > max_bytes {
        return broken(path, format!("{bytes} bytes of JSON, at most {max_bytes}"));
    }
    Ok(())
}

fn broken(path: &str, problem: impl Display) -> Checked {
    Err(LimitError {
        path: path.to_owned(),
        problem: problem.to_string(),
    })
}
