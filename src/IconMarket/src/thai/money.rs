//! Money in Thai Baht (CON-11), exact to the satang (DR-01).

use std::fmt;

use serde::{Deserialize, Serialize};

/// An exact amount of Thai Baht, held in satang (1/100 baht) so arithmetic never rounds
/// (DR-01). Displays as `฿1,250` for whole baht and `฿12.50` otherwise (FR-UI-07). A draft in a
/// guided flow keeps it as its satang.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money(i64);

impl Money {
    pub const ZERO: Self = Self(0);

    pub const fn from_satang(satang: i64) -> Self {
        Self(satang)
    }

    /// Panics if the amount does not fit, which no real price comes near.
    pub const fn from_baht(baht: i64) -> Self {
        Self(baht.checked_mul(100).expect("amount in range"))
    }

    pub const fn satang(self) -> i64 {
        self.0
    }

    /// `quantity` of something at this price: a line's total. Panics if it does not fit, which
    /// no cart or order comes near.
    pub fn times(self, quantity: i32) -> Self {
        let total = self.0.checked_mul(i64::from(quantity));
        Self(total.expect("amount in range"))
    }
}

/// The total of the amounts, as a cart's or an order's subtotal. Panics if it does not fit.
impl std::iter::Sum for Money {
    fn sum<I: Iterator<Item = Self>>(mut amounts: I) -> Self {
        let total = amounts.try_fold(0_i64, |total, amount| total.checked_add(amount.0));
        Self(total.expect("amount in range"))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 0 {
            f.write_str("-")?;
        }
        let satang = self.0.unsigned_abs();
        write!(f, "฿{}", group_thousands(satang / 100))?;
        match satang % 100 {
            0 => Ok(()),
            rest => write!(f, ".{rest:02}"),
        }
    }
}

/// `1250` → `1,250`, as amounts and counts are shown
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_amounts_have_no_decimals() {
        assert_eq!(Money::ZERO.to_string(), "฿0");
        assert_eq!(Money::from_baht(20).to_string(), "฿20");
        assert_eq!(Money::from_baht(1_250).to_string(), "฿1,250");
        assert_eq!(Money::from_baht(100_000).to_string(), "฿100,000");
        assert_eq!(Money::from_baht(1_000_000).to_string(), "฿1,000,000");
    }

    #[test]
    fn other_amounts_have_two_decimals() {
        assert_eq!(Money::from_satang(1).to_string(), "฿0.01");
        assert_eq!(Money::from_satang(1_250).to_string(), "฿12.50");
        assert_eq!(Money::from_satang(125_050).to_string(), "฿1,250.50");
    }

    #[test]
    fn a_line_is_its_price_times_its_quantity() {
        assert_eq!(Money::from_baht(110).times(2), Money::from_baht(220));
        assert_eq!(
            Money::from_satang(1_250).times(3),
            Money::from_satang(3_750)
        );
        assert_eq!(Money::from_baht(100_000).times(0), Money::ZERO);
    }

    #[test]
    fn amounts_add_up() {
        let lines = [Money::from_baht(220), Money::from_satang(1_250)];
        assert_eq!(lines.into_iter().sum::<Money>(), Money::from_satang(23_250));
        assert_eq!(std::iter::empty::<Money>().sum::<Money>(), Money::ZERO);
    }

    #[test]
    fn negative_amounts_lead_with_a_minus() {
        assert_eq!(Money::from_satang(-1_050).to_string(), "-฿10.50");
    }
}
