//! Thai phone numbers typed by users (FR-ONB-04): mobiles of 10 digits starting `06`, `08` or
//! `09`, and landlines of 9 digits starting `02`–`07`, in Arabic or Thai digits, with or without
//! dashes, spaces or the country code: `081-234-5678`, `๐๘๑ ๒๓๔ ๕๖๗๘`, `+66 81 234 5678`.
//!
//! They are kept as their digits alone, `0812345678`, and shown grouped as people write them.

use super::arabic_digits;

/// The digits of a valid Thai phone number, such as `0812345678`; `None` if `text` is none.
pub fn parse_phone(text: &str) -> Option<String> {
    let typed: String = arabic_digits(text)
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '.' | '(' | ')'))
        .collect();
    let digits = match typed.strip_prefix('+').unwrap_or(&typed).strip_prefix("66") {
        // The country code stands for the leading 0, which some still type after it
        Some(national) if national.starts_with('0') => national.to_owned(),
        Some(national) => format!("0{national}"),
        None if typed.starts_with('+') => return None,
        None => typed,
    };
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let valid = matches!(
        (digits.len(), digits.as_bytes()),
        (10, [b'0', b'6' | b'8' | b'9', ..]) | (9, [b'0', b'2'..=b'7', ..])
    );
    valid.then_some(digits)
}

/// `digits` of a phone number as people write them: `081-234-5678`, `02-123-4567` for
/// Bangkok, and `053-123-456` for other landlines. Anything else is shown as it is.
pub fn format_phone(digits: &str) -> String {
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return digits.to_owned();
    }
    let groups: &[usize] = match (digits.len(), digits.starts_with("02")) {
        (10, _) => &[3, 3, 4],
        (9, true) => &[2, 3, 4],
        (9, false) => &[3, 3, 3],
        _ => return digits.to_owned(),
    };
    let mut parts = Vec::with_capacity(groups.len());
    let mut rest = digits;
    for &len in groups {
        let (part, after) = rest.split_at(len);
        parts.push(part);
        rest = after;
    }
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobiles_and_landlines_however_typed() {
        for (typed, digits) in [
            ("0812345678", "0812345678"),
            ("081-234-5678", "0812345678"),
            (" 081 234 5678 ", "0812345678"),
            ("081.234.5678", "0812345678"),
            ("(081) 234-5678", "0812345678"),
            ("๐๘๑ ๒๓๔ ๕๖๗๘", "0812345678"),
            ("+66 81 234 5678", "0812345678"),
            ("+66812345678", "0812345678"),
            ("66812345678", "0812345678"),
            ("+66 081 234 5678", "0812345678"),
            ("0612345678", "0612345678"),
            ("0912345678", "0912345678"),
            ("021234567", "021234567"),
            ("02-123-4567", "021234567"),
            ("053 123 456", "053123456"),
            ("+66 2 123 4567", "021234567"),
            ("076-123-456", "076123456"),
        ] {
            assert_eq!(parse_phone(typed).as_deref(), Some(digits), "{typed:?}");
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for typed in [
            "",
            "   ",
            "12345",
            "1669",
            // A mobile prefix needs 10 digits, a landline 9
            "081234567",
            "08123456789",
            "0212345678",
            "02123456",
            "0012345678",
            "0112345678",
            "812345678",
            "+1 202 555 0123",
            "+081 234 5678",
            "081234567x",
            "เบอร์ 0812345678",
            "081/234/5678",
            "081-234-5678 ต่อ 12",
        ] {
            assert_eq!(parse_phone(typed), None, "{typed:?}");
        }
    }

    #[test]
    fn numbers_are_grouped_as_people_write_them() {
        assert_eq!(format_phone("0812345678"), "081-234-5678");
        assert_eq!(format_phone("021234567"), "02-123-4567");
        assert_eq!(format_phone("053123456"), "053-123-456");
        // Whatever an earlier release or an admin stored is shown as it is
        assert_eq!(format_phone("1669"), "1669");
        assert_eq!(format_phone("๐๘๑๒๓๔๕๖๗๘"), "๐๘๑๒๓๔๕๖๗๘");
    }
}
