//! Postback data (FR-UI-13, NFR-SEC-03): what a button sends back when tapped. It names an
//! action and carries only opaque identifiers, as `action` or `action?key=value&key=value`:
//! `my_orders`, `browse?page=2`, `confirm?k=checkout&pa=6f1c…`.
//!
//! Values are built only from numbers (IDs, page numbers), random UUIDs and names fixed in code,
//! and read back only as those, so a postback can neither carry prices, amounts or personal data
//! nor be trusted for them. Anyone can send any postback data, so parsing is strict, and a
//! postback is only ever a request. Every handler of one acts for the
//! [`Turn::actor`](super::Turn::actor), loaded afresh for each event, and before acting re-checks
//! the actor's authorization ([`authorize`](crate::users::access::authorize)) and that what it acts
//! on still exists, in the state and at the price the button was sent for. If anything changed,
//! it shows the current state instead of acting. Confirming a proposed change goes through
//! [`pending::confirm`](super::pending::confirm), which does all of this.

use std::borrow::Cow;
use std::fmt;

use sea_orm::prelude::Uuid;

use crate::line::Action;
use crate::line::limits::ACTION_TEXT_CHARS;
use crate::thai::Label;

/// The data of a postback button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Postback {
    action: Cow<'static, str>,
    params: Vec<(Cow<'static, str>, String)>,
}

impl Postback {
    /// A postback for `action`: lowercase ASCII letters, digits, `_` and `.`, starting with a
    /// letter.
    pub fn new(action: &'static str) -> Self {
        debug_assert!(is_action(action), "malformed postback action {action:?}");
        Self {
            action: Cow::Borrowed(action),
            params: Vec::new(),
        }
    }

    /// Adds a number: an ID, a page number or the index of a choice.
    pub fn with_int(self, key: &'static str, value: i64) -> Self {
        self.with(key, value.to_string())
    }

    /// Adds a random ID, such as a pending action's (NFR-SEC-04).
    pub fn with_uuid(self, key: &'static str, value: Uuid) -> Self {
        self.with(key, value.to_string())
    }

    /// Adds a name fixed in code, such as a choice (`pickup`) or a kind (`checkout`).
    pub fn with_name(self, key: &'static str, value: &'static str) -> Self {
        debug_assert!(is_value(value), "malformed postback value {value:?}");
        self.with(key, value.to_owned())
    }

    fn with(mut self, key: &'static str, value: String) -> Self {
        debug_assert!(
            is_key(key) && self.value(key).is_none(),
            "malformed or repeated postback key {key:?}"
        );
        self.params.push((Cow::Borrowed(key), value));
        self
    }

    /// The postback `data` sent: `None` unless it is data that a `Postback` displays as.
    pub fn parse(data: &str) -> Option<Self> {
        if data.len() > ACTION_TEXT_CHARS {
            return None;
        }
        let (action, query) = match data.split_once('?') {
            Some((action, query)) => (action, Some(query)),
            None => (data, None),
        };
        if !is_action(action) {
            return None;
        }
        let mut postback = Self {
            action: Cow::Owned(action.to_owned()),
            params: Vec::new(),
        };
        for pair in query.into_iter().flat_map(|query| query.split('&')) {
            let (key, value) = pair.split_once('=')?;
            if !is_key(key) || !is_value(value) || postback.value(key).is_some() {
                return None;
            }
            let param = (Cow::Owned(key.to_owned()), value.to_owned());
            postback.params.push(param);
        }
        Some(postback)
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        self.value(key)?.parse().ok()
    }

    pub fn uuid(&self, key: &str) -> Option<Uuid> {
        Uuid::try_parse(self.value(key)?).ok()
    }

    pub fn name(&self, key: &str) -> Option<&str> {
        self.value(key)
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.as_str())
    }

    /// A button sending this postback. The chat shows `label` as if the user had typed it, so
    /// the conversation reads as what they tapped.
    pub fn button(&self, label: Label) -> Action {
        Action::Postback {
            display_text: Some(label.as_str().to_owned()),
            label,
            data: self.to_string(),
        }
    }
}

impl fmt::Display for Postback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.action)?;
        for (i, (key, value)) in self.params.iter().enumerate() {
            let separator = if i == 0 { '?' } else { '&' };
            write!(f, "{separator}{key}={value}")?;
        }
        Ok(())
    }
}

/// `[a-z][a-z0-9_.]*`
fn is_action(text: &str) -> bool {
    starts_lowercase(text)
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

/// `[a-z][a-z0-9_]*`
fn is_key(text: &str) -> bool {
    starts_lowercase(text)
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `[A-Za-z0-9_.-]+`: numbers, UUIDs and names, nothing a person could have typed
fn is_value(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn starts_lowercase(text: &str) -> bool {
    text.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line::limits;

    const ID: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

    #[test]
    fn postbacks_round_trip() {
        let id = Uuid::parse_str(ID).unwrap();
        let confirm = Postback::new("confirm")
            .with_name("k", "product.create")
            .with_uuid("pa", id);
        assert_eq!(
            confirm.to_string(),
            format!("confirm?k=product.create&pa={ID}")
        );
        let parsed = Postback::parse(&confirm.to_string()).unwrap();
        assert_eq!(parsed, confirm);
        assert_eq!(parsed.action(), "confirm");
        assert_eq!(parsed.name("k"), Some("product.create"));
        assert_eq!(parsed.uuid("pa"), Some(id));
        assert_eq!(parsed.int("pa"), None);
        assert_eq!(parsed.uuid("missing"), None);

        let page = Postback::new("browse")
            .with_int("category", 3)
            .with_int("page", 2);
        assert_eq!(page.to_string(), "browse?category=3&page=2");
        let parsed = Postback::parse("browse?category=3&page=2").unwrap();
        assert_eq!(
            (parsed.int("category"), parsed.int("page")),
            (Some(3), Some(2))
        );

        let bare = Postback::parse("my_orders").unwrap();
        assert_eq!(bare, Postback::new("my_orders"));
        assert_eq!(bare.to_string(), "my_orders");
    }

    /// NFR-SEC-03: only an action name and IDs get through, so nothing typed or priced does.
    #[test]
    fn anything_else_is_refused() {
        let long = format!("browse?page={}", "1".repeat(limits::ACTION_TEXT_CHARS));
        for data in [
            "",
            "?page=2",
            "Browse",
            "2browse",
            "browse page",
            "browse?",
            "browse?page",
            "browse?page=",
            "browse?=2",
            "browse?Page=2",
            "browse?page=2&",
            "browse?page=2&page=3",
            "browse?page=2?x=1",
            "browse?q=ไข่ไก่",
            "browse?q=two words",
            "browse?price=฿20",
            "browse?q=a%20b",
            "ซื้อของ",
            long.as_str(),
        ] {
            assert_eq!(Postback::parse(data), None, "{data:?}");
        }
    }

    #[test]
    fn values_are_read_only_as_what_they_are() {
        let postback = Postback::parse("order.accept?o=12&pa=not-a-uuid&n=x1").unwrap();
        assert_eq!(postback.int("o"), Some(12));
        assert_eq!(postback.uuid("pa"), None);
        assert_eq!(postback.int("n"), None);
    }

    #[test]
    fn buttons_show_their_label_and_fit() {
        let id = Uuid::parse_str(ID).unwrap();
        let postback = Postback::new("confirm")
            .with_name("k", "checkout")
            .with_uuid("pa", id);
        let label = Label::from_static("ยืนยันสั่งซื้อ");
        let button = postback.button(label.clone());
        assert_eq!(
            button,
            Action::Postback {
                label,
                data: format!("confirm?k=checkout&pa={ID}"),
                display_text: Some("ยืนยันสั่งซื้อ".to_owned()),
            }
        );
        let message = crate::line::Message::text("สรุป")
            .with_quick_reply(crate::line::QuickReply::new([button]));
        limits::check(&[message]).unwrap();
    }
}
