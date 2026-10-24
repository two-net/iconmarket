//! Admin (SRS §2.1): approvals, moderation queue, users, categories, statistics and
//! announcements. Built by #21, #23, #31, #38 and #49. Every admin action and sensitive action
//! is written to the [`audit`] log (FR-ADM-08).

pub mod audit;
