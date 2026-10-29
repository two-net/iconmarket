//! Admin (SRS §2.1): approvals, moderation queue, users, categories, statistics and
//! announcements. Built by #21, #23, #31, #38 and #49. Every admin action and sensitive action
//! is written to the [`audit`] log (FR-ADM-08).
//!
//! [`approvals`] is "อนุมัติร้าน" (FR-ADM-02), [`review`] "รายการรอตรวจ" (FR-ADM-03), [`accounts`]
//! "ผู้ใช้" (FR-ADM-04), [`categories`] "หมวดหมู่" (FR-ADM-05) and [`statistics`] "สถิติ"
//! (FR-ADM-06). Each decision with a [`reason`] tells it to whoever it affects. Admins change
//! things from these menus only, never through the AI (FR-AI-17), and nothing here changes an
//! order's items or amounts (FR-ADM-09).

pub mod accounts;
pub mod approvals;
pub mod audit;
pub mod categories;
pub mod reason;
pub mod review;
pub mod statistics;
