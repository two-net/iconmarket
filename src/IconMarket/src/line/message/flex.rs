//! The part of LINE's Flex Messages that IconMarket's cards use: bubbles and carousels of
//! boxes, text, images, buttons and separators. Card layouts live with the message
//! templates (NFR-MNT-03); add properties here when a card needs one.
//!
//! Fields are public and optional, so a component is its constructor plus what differs:
//! `Text { wrap: Some(true), ..Text::new("ไข่ไก่สด") }`.

use serde::Serialize;
use url::Url;

use super::Action;

/// The `contents` of a Flex message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FlexContainer {
    Bubble(Box<Bubble>),
    Carousel(Carousel),
}

impl From<Bubble> for FlexContainer {
    fn from(bubble: Bubble) -> Self {
        Self::Bubble(Box::new(bubble))
    }
}

impl From<Carousel> for FlexContainer {
    fn from(carousel: Carousel) -> Self {
        Self::Carousel(carousel)
    }
}

/// One card.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "type", rename = "bubble")]
pub struct Bubble {
    /// Every bubble of a carousel must have the same size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<BubbleSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<FlexBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hero: Option<Hero>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<FlexBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<FlexBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

/// Cards side by side, scrolled sideways: at most 12 (FR-LINE-11).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "carousel")]
pub struct Carousel {
    pub contents: Vec<Bubble>,
}

impl Carousel {
    pub fn new(bubbles: Vec<Bubble>) -> Self {
        Self { contents: bubbles }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BubbleSize {
    Nano,
    Micro,
    Deca,
    Hecto,
    Kilo,
    Mega,
    Giga,
}

/// The hero block: a picture across the top of a card, or a box.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Hero {
    Image(Image),
    Box(FlexBox),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Component {
    Box(FlexBox),
    Text(Text),
    Image(Image),
    Button(Button),
    Separator(Separator),
}

macro_rules! into_component {
    ($($kind:ident),*) => {
        $(
            impl From<$kind> for Component {
                fn from(component: $kind) -> Self {
                    Self::$kind(component)
                }
            }
        )*
    };
}

into_component!(Text, Image, Button, Separator);

impl From<FlexBox> for Component {
    fn from(component: FlexBox) -> Self {
        Self::Box(component)
    }
}

/// Lays out other components in a row or a column.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "box", rename_all = "camelCase")]
pub struct FlexBox {
    pub layout: Layout,
    pub contents: Vec<Component>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spacing: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_all: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corner_radius: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flex: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub justify_content: Option<Justify>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

impl FlexBox {
    pub fn vertical(contents: impl IntoIterator<Item = Component>) -> Self {
        Self::new(Layout::Vertical, contents)
    }

    pub fn horizontal(contents: impl IntoIterator<Item = Component>) -> Self {
        Self::new(Layout::Horizontal, contents)
    }

    /// A row whose texts share a baseline, e.g. a label and a price
    pub fn baseline(contents: impl IntoIterator<Item = Component>) -> Self {
        Self::new(Layout::Baseline, contents)
    }

    fn new(layout: Layout, contents: impl IntoIterator<Item = Component>) -> Self {
        Self {
            layout,
            contents: contents.into_iter().collect(),
            spacing: None,
            margin: None,
            padding_all: None,
            background_color: None,
            corner_radius: None,
            flex: None,
            justify_content: None,
            action: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    Horizontal,
    Vertical,
    Baseline,
}

/// Spacing, margin, padding and corner radius, from none to xxl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Spacing {
    None,
    Xs,
    Sm,
    Md,
    Lg,
    Xl,
    Xxl,
}

/// Placement of a box's children along its layout direction, when they leave space free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Justify {
    FlexStart,
    Center,
    FlexEnd,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "text", rename_all = "camelCase")]
pub struct Text {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<Weight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gravity: Option<Gravity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flex: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            size: None,
            weight: None,
            color: None,
            wrap: None,
            max_lines: None,
            align: None,
            gravity: None,
            margin: None,
            flex: None,
            action: None,
        }
    }
}

/// Text and image sizes, from xxs to 5xl; `Full` is for images only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Size {
    #[serde(rename = "xxs")]
    Xxs,
    #[serde(rename = "xs")]
    Xs,
    #[serde(rename = "sm")]
    Sm,
    #[serde(rename = "md")]
    Md,
    #[serde(rename = "lg")]
    Lg,
    #[serde(rename = "xl")]
    Xl,
    #[serde(rename = "xxl")]
    Xxl,
    #[serde(rename = "3xl")]
    X3l,
    #[serde(rename = "4xl")]
    X4l,
    #[serde(rename = "5xl")]
    X5l,
    #[serde(rename = "full")]
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Weight {
    Regular,
    Bold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Start,
    End,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Gravity {
    Top,
    Bottom,
    Center,
}

/// A JPEG or PNG from our `/img` links (FR-LINE-01).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "image", rename_all = "camelCase")]
pub struct Image {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    /// Width to height, e.g. `"20:13"`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_mode: Option<AspectMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flex: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

impl Image {
    pub fn new(url: &Url) -> Self {
        Self {
            url: url.to_string(),
            size: None,
            aspect_ratio: None,
            aspect_mode: None,
            margin: None,
            flex: None,
            action: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AspectMode {
    /// The whole image, letterboxed
    Fit,
    /// Fills the area, cropping the image
    Cover,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "button", rename_all = "camelCase")]
pub struct Button {
    pub action: Action,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<ButtonStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<ButtonHeight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flex: Option<u32>,
}

impl Button {
    pub fn new(action: Action) -> Self {
        Self {
            action,
            style: None,
            color: None,
            height: None,
            margin: None,
            flex: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonStyle {
    /// Filled with the main colour
    Primary,
    /// Filled with a light colour
    Secondary,
    /// Text only
    Link,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonHeight {
    Sm,
    Md,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "type", rename = "separator")]
pub struct Separator {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin: Option<Spacing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}
