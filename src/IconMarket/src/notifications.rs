//! Notifications (SRS §2.1, §4.8): message templates, and the push notifications of Table
//! 4.8-A with their switches, per-recipient batching, push log and retries ([`push`], #15).
//! The rich menus for each role are provisioned at startup and linked to users
//! ([`rich_menus`], #17).
//!
//! A feature notifies through the [`Outbox`] of its processing step:
//! [`Turn::notify`](crate::dispatcher::Turn::notify) while handling an event, or an outbox of
//! its own in a job, queued in the job's transaction and sent once that commits. The village
//! group digest, for a group rather than users, is queued with [`queue_group`].

mod push;
mod rich_menus;
pub mod templates;
#[cfg(test)]
mod tests;

#[allow(
    unused_imports,
    reason = "the features that notify are built by later issues"
)]
pub use push::{
    Kind, Outbox, PushStats, Queued, Retried, To, log_reply_pushed, queue_group, reaches, retry,
    send, stats,
};
pub use rich_menus::{RichMenus, provision_rich_menus, relink_rich_menus};
