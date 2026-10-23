//! LINE message objects as IconMarket sends them: text, images and Flex cards, with Quick
//! Replies and actions. They serialize to LINE's JSON; [`limits::check`](super::limits::check)
//! runs before anything is sent.

pub mod flex;

use serde::Serialize;
use url::Url;

use self::flex::FlexContainer;
use super::limits::ALT_TEXT_CHARS;
use crate::thai::{self, Label};

/// One message object of a reply or push; a request holds up to five (FR-LINE-11).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(flatten)]
    pub body: Body,
    /// Shown under the last message of a request only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_reply: Option<QuickReply>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Body {
    Text {
        text: String,
    },
    Image {
        original_content_url: String,
        preview_image_url: String,
    },
    Flex {
        /// The notification and chat-list preview
        alt_text: String,
        contents: FlexContainer,
    },
}

impl Message {
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(Body::Text { text: text.into() })
    }

    /// An image served from our `/img` links (FR-LINE-01).
    pub fn image(original: &Url, preview: &Url) -> Self {
        Self::new(Body::Image {
            original_content_url: original.to_string(),
            preview_image_url: preview.to_string(),
        })
    }

    /// A card or carousel. The alt text is only a preview, so a long one is
    /// [shortened](thai::shorten) to LINE's limit instead of being refused.
    pub fn flex(alt_text: impl Into<String>, contents: impl Into<FlexContainer>) -> Self {
        Self::new(Body::Flex {
            alt_text: thai::shorten(alt_text.into(), ALT_TEXT_CHARS),
            contents: contents.into(),
        })
    }

    pub fn with_quick_reply(mut self, quick_reply: QuickReply) -> Self {
        self.quick_reply = Some(quick_reply);
        self
    }

    fn new(body: Body) -> Self {
        Self {
            body,
            quick_reply: None,
        }
    }
}

/// Buttons under a message, for the next step (FR-UI-03).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QuickReply {
    pub items: Vec<QuickReplyItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "action")]
pub struct QuickReplyItem {
    pub action: Action,
}

impl QuickReply {
    pub fn new(actions: impl IntoIterator<Item = Action>) -> Self {
        Self {
            items: actions
                .into_iter()
                .map(|action| QuickReplyItem { action })
                .collect(),
        }
    }
}

/// What a button, Quick Reply, card or rich-menu area does when tapped. Labels are at most
/// 20 characters, which fits everywhere LINE shows one (NFR-USE-04).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// Sends `data` back as a postback event: an action name and opaque IDs only, never
    /// prices or personal data (FR-UI-13, NFR-SEC-03).
    Postback {
        label: Label,
        data: String,
        /// Shown in the chat as if the user had typed it
        #[serde(skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
    },
    /// Sends `text` as if the user had typed it.
    Message { label: Label, text: String },
    /// Opens a link: `https:`, `http:`, `line:` or `tel:`.
    Uri { label: Label, uri: String },
    /// Opens the camera. Quick Reply only.
    Camera { label: Label },
    /// Opens the photo picker. Quick Reply only.
    CameraRoll { label: Label },
    /// Opens the location picker. Quick Reply only.
    Location { label: Label },
    /// Switches to the rich menu behind an alias, sending `data` as a postback. Rich menus
    /// only (FR-UI-10).
    #[serde(rename = "richmenuswitch")]
    RichMenuSwitch {
        label: Label,
        rich_menu_alias_id: String,
        data: String,
    },
}

impl Action {
    pub fn postback(label: Label, data: impl Into<String>) -> Self {
        Self::Postback {
            label,
            data: data.into(),
            display_text: None,
        }
    }

    pub fn message(label: Label, text: impl Into<String>) -> Self {
        Self::Message {
            label,
            text: text.into(),
        }
    }

    pub fn label(&self) -> &Label {
        match self {
            Self::Postback { label, .. }
            | Self::Message { label, .. }
            | Self::Uri { label, .. }
            | Self::Camera { label }
            | Self::CameraRoll { label }
            | Self::Location { label }
            | Self::RichMenuSwitch { label, .. } => label,
        }
    }
}
