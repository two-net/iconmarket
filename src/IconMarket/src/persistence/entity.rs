//! The entity models of SRS §5.1, and the schema versions applied (MIG-09), one module per
//! table. They are the single source of truth for the schema (MIG-01); change the schema by
//! changing them.
//!
//! The data rules (§5.2) are applied in the models:
//!
//! - Money is [`Money`](crate::thai::Money), stored as `numeric(12, 2)` (DR-01).
//! - Timestamps are `DateTimeUtc`, stored as `timestamp with time zone` and converted to the
//!   business time zone only for display and day boundaries (DR-02).
//! - Enums are stored as text with the SRS names (`PENDING`, `REFUND_DUE`), not as PostgreSQL
//!   enum types, so a later release can add a value without altering a type.
//! - Shops and products are soft-deleted; orders are never deleted, only anonymized (DR-04).
//! - Every reference is a foreign key (DR-06), except owner and target IDs that point at one
//!   of several tables, and `orders.checkout_key`, which outlives its pending action.
//! - Text is unbounded and UTF-8 (DR-09); length limits are checked by the flows.
//! - Personal-data fields are listed by [`PersonalData`] (DR-10) and are nullable, so account
//!   deletion can clear them.
//! - Nothing is partitioned (DR-08).

#![allow(
    dead_code,
    reason = "the models are queried by the flows that later issues build"
)]

use sea_orm::EntityTrait;

pub mod ai_call_log;
pub mod ai_message;
pub mod audit_log;
pub mod cart_item;
pub mod category;
pub mod counter;
pub mod image;
pub mod job;
pub mod join_attempt;
pub mod order;
pub mod order_event;
pub mod order_item;
pub mod payment_slip;
pub mod pending_action;
pub mod product;
pub mod push_log;
pub mod rating;
pub mod report;
pub mod schema_version;
pub mod session;
pub mod shop;
pub mod user;
pub mod webhook_event;

/// An entity holding personal data, so that access requests can show it and account deletion
/// can clear it (DR-10, FR-ONB-05, FR-ONB-07).
pub trait PersonalData: EntityTrait {
    /// The columns holding personal data.
    const FIELDS: &'static [Self::Column];
}
