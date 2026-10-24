//! Thai text in and out (CON-11), shared by every flow: numbers typed with Arabic or Thai
//! digits, phone numbers, money in baht, dates in the Buddhist Era and labels that fit LINE's
//! buttons.
//!
//! No wording lives here; the message templates (`notifications::templates`) hold it.

#![allow(
    dead_code,
    reason = "shared helpers for the flows that later issues build"
)]

mod date;
mod label;
mod money;
mod number;
mod phone;

pub use date::{format_date, format_date_time, local_instant};
pub use label::{Label, shorten};
pub use money::Money;
#[allow(
    unused_imports,
    reason = "the flows that read numbers are built by later issues"
)]
pub use number::{NumberError, arabic_digits, parse_money, parse_whole};
pub use phone::{format_phone, parse_phone};
