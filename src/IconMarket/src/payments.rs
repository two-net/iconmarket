//! Payments (SRS §2.1): payment method and status, PromptPay details and slips.
//! Built by #35, #40 and #41.

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder};

use crate::persistence::entity::order::{self, PaymentMethod, PaymentStatus, Status};
use crate::thai;

/// Buyer `buyer_id`'s latest order that its seller accepted and that awaits a PromptPay
/// transfer: the order a photo the buyer sends may be the slip of (FR-UI-14, FR-PAY-05).
pub async fn awaiting_transfer(
    db: &impl ConnectionTrait,
    buyer_id: i64,
) -> Result<Option<order::Model>, DbErr> {
    order::Entity::find()
        .filter(order::Column::BuyerId.eq(buyer_id))
        .filter(order::Column::Status.is_in([Status::Accepted, Status::Ready, Status::Delivering]))
        .filter(order::Column::PaymentMethod.eq(PaymentMethod::PromptPay))
        .filter(order::Column::PaymentStatus.eq(PaymentStatus::Unpaid))
        .order_by_desc(order::Column::CreatedAt)
        .one(db)
        .await
}

/// The digits of the PromptPay ID typed as `text`, as a shop gives it (FR-SHOP-01): a mobile
/// number, a 13-digit citizen or tax ID whose check digit is right, or a 15-digit e-wallet ID.
/// Typed in Arabic or Thai digits, with or without dashes and spaces: `081-234-5678`,
/// `1-1017-00203-45-0`. `None` if it is none of these.
pub fn parse_promptpay_id(text: &str) -> Option<String> {
    // Landlines can't receive PromptPay
    if let Some(mobile) = thai::parse_phone(text).filter(|digits| digits.len() == 10) {
        return Some(mobile);
    }
    let digits: String = thai::arabic_digits(text)
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let valid = match digits.len() {
        13 => id_check_digit(&digits[..12]) == digits.as_bytes()[12] - b'0',
        15 => true,
        _ => false,
    };
    valid.then_some(digits)
}

/// `digits` of a PromptPay ID as people write them: a mobile number as phones are,
/// `081-234-5678`, and a citizen or tax ID as on the card, `1-1017-00203-45-0`. An e-wallet ID,
/// or anything else, is shown as it is.
pub fn format_promptpay_id(digits: &str) -> String {
    match digits.len() {
        10 => thai::format_phone(digits),
        13 if digits.is_ascii() => {
            let parts = [
                &digits[..1],
                &digits[1..5],
                &digits[5..10],
                &digits[10..12],
                &digits[12..],
            ];
            parts.join("-")
        }
        _ => digits.to_owned(),
    }
}

/// The check digit of a Thai citizen or tax ID whose first 12 digits are `first`.
fn id_check_digit(first: &str) -> u8 {
    let sum: u32 = first
        .bytes()
        .zip((2..=13).rev())
        .map(|(digit, weight)| u32::from(digit - b'0') * weight)
        .sum();
    u8::try_from((11 - sum % 11) % 10).expect("a digit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promptpay_ids_however_typed() {
        for (typed, digits) in [
            ("0812345678", "0812345678"),
            ("081-234-5678", "0812345678"),
            ("๐๘๑ ๒๓๔ ๕๖๗๘", "0812345678"),
            ("+66 81 234 5678", "0812345678"),
            ("1101700203450", "1101700203450"),
            ("1-1017-00203-45-0", "1101700203450"),
            ("0105536123458", "0105536123458"),
            ("๓๑๐๑๐๐๑๒๓๔๕๖๕", "3101001234565"),
            ("004999012345678", "004999012345678"),
        ] {
            assert_eq!(
                parse_promptpay_id(typed).as_deref(),
                Some(digits),
                "{typed}"
            );
        }
    }

    #[test]
    fn promptpay_ids_are_shown_as_people_write_them() {
        assert_eq!(format_promptpay_id("0812345678"), "081-234-5678");
        assert_eq!(format_promptpay_id("1101700203450"), "1-1017-00203-45-0");
        assert_eq!(format_promptpay_id("004999012345678"), "004999012345678");
    }

    #[test]
    fn anything_else_is_refused() {
        for typed in [
            "",
            "021234567",
            "0212345678",
            "081234567",
            "1101700203451",
            "110170020345",
            "11017002034500",
            "0812345678x",
            "พร้อมเพย์ 0812345678",
            "1101700203450.0",
        ] {
            assert_eq!(parse_promptpay_id(typed), None, "{typed}");
        }
    }
}
