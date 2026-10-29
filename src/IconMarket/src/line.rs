//! The LINE Messaging API, outbound (SRS §3.2): replies and pushes, the loading animation,
//! user content and profiles, and rich menus (FR-LINE-07 – FR-LINE-12). Every module that
//! talks to users goes through it; the webhook receiver is the inbound side.
//!
//! LINE's limits are checked before anything is sent ([`limits`]). LINE can change them and
//! its behaviour (A-07), so both follow its current documentation:
//! <https://developers.line.biz/en/reference/messaging-api/>.

#![allow(
    dead_code,
    reason = "the flows that talk to users are built by later issues"
)]

mod client;
mod delivery;
mod error;
pub mod limits;
mod message;
mod rich_menu;
#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;

pub use client::Client;
#[allow(
    unused_imports,
    reason = "the flows that talk to users are built by later issues"
)]
pub use {
    client::{BotInfo, Content, Profile},
    delivery::{Delivery, Importance, Pushed, RetryKey},
    error::{Error, LimitError},
    message::{Action, Body, Message, QuickReply, QuickReplyItem, flex},
    rich_menu::{Alias, Area, Bounds, ImageType, RichMenu, RichMenuInfo, Size},
};
