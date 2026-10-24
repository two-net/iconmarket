//! Admin (SRS §2.1): approvals, moderation queue, users, categories, statistics and
//! announcements. Built by #21, #23, #31, #38 and #49. Every admin action and sensitive action
//! is written to the [`audit`] log (FR-ADM-08).
//!
//! [`approvals`] is "อนุมัติร้าน" (FR-ADM-02), and [`categories`] is "หมวดหมู่" (FR-ADM-05).

pub mod approvals;
pub mod audit;
pub mod categories;
