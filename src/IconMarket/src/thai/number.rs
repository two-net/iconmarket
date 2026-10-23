//! Numbers typed by users (FR-UI-06): Arabic or Thai digits (๐–๙), commas between
//! thousands, and the currency as `฿` or `บาท`, e.g. `๒๐`, `1,200 บาท` or `๑,๒๐๐.๕๐ บาท`.
//!
//! Range rules, such as a product's price limits, belong to the caller.

use super::Money;

/// Why text was not accepted as a number. The templates explain it in Thai; it is never
/// shown as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberError {
    /// Empty, letters, a sign, misplaced commas or a second dot
    NotANumber,
    /// More than 2 decimals in an amount of money
    TooManyDecimals,
    /// A fraction where only a whole number makes sense
    NotWhole,
    /// More digits than can be held
    TooLarge,
}

/// An amount of money with at most 2 decimals: `20`, `12.5`, `1,200 บาท`, `฿๑,๒๐๐.๕๐`.
pub fn parse_money(text: &str) -> Result<Money, NumberError> {
    let Digits { whole, fraction } = digits(text)?;
    let satang = match fraction.as_bytes() {
        [] => 0,
        [tenths] => i64::from(tenths - b'0') * 10,
        [tenths, hundredths] => i64::from(tenths - b'0') * 10 + i64::from(hundredths - b'0'),
        _ => return Err(NumberError::TooManyDecimals),
    };
    whole
        .parse::<i64>()
        .ok()
        .and_then(|baht| baht.checked_mul(100))
        .and_then(|whole| whole.checked_add(satang))
        .map(Money::from_satang)
        .ok_or(NumberError::TooLarge)
}

/// A whole number, such as a quantity or a stock count: `2`, `๑๐`, `1,000`.
pub fn parse_whole(text: &str) -> Result<u32, NumberError> {
    let Digits { whole, fraction } = digits(text)?;
    if !fraction.is_empty() {
        return Err(NumberError::NotWhole);
    }
    whole.parse().map_err(|_| NumberError::TooLarge)
}

/// The ASCII digits before and after the decimal point. Trailing zeros after the point
/// are dropped, since they don't change the value.
struct Digits {
    whole: String,
    fraction: String,
}

fn digits(text: &str) -> Result<Digits, NumberError> {
    let text = text.trim();
    let text = text.strip_prefix('฿').unwrap_or(text).trim_start();
    let text = text.strip_suffix("บาท").unwrap_or(text).trim_end();
    let text: String = text.chars().map(arabic).collect();

    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) if is_digits(fraction) => (whole, fraction),
        Some(_) => return Err(NumberError::NotANumber),
        None => (text.as_str(), ""),
    };
    Ok(Digits {
        whole: ungroup(whole).ok_or(NumberError::NotANumber)?,
        fraction: fraction.trim_end_matches('0').to_owned(),
    })
}

/// `๐`–`๙` as `0`–`9`; anything else unchanged.
fn arabic(c: char) -> char {
    match c {
        '๐'..='๙' => char::from(b'0' + (u32::from(c) - u32::from('๐')) as u8),
        _ => c,
    }
}

/// The integer part without its commas, which must group the digits in threes: `1,200`
/// but not `1,20`, which may be a typo for `1.20`.
fn ungroup(whole: &str) -> Option<String> {
    let mut groups = whole.split(',');
    let first = groups.next()?;
    if !is_digits(first) || (whole.contains(',') && first.len() > 3) {
        return None;
    }
    let mut digits = first.to_owned();
    for group in groups {
        if group.len() != 3 || !is_digits(group) {
            return None;
        }
        digits.push_str(group);
    }
    Some(digits)
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn money(text: &str) -> i64 {
        parse_money(text)
            .unwrap_or_else(|e| panic!("{text:?}: {e:?}"))
            .satang()
    }

    #[test]
    fn money_in_arabic_or_thai_digits() {
        assert_eq!(money("20"), 2_000);
        assert_eq!(money("๒๐"), 2_000);
        assert_eq!(money("1๒"), 1_200);
        assert_eq!(money("12.5"), 1_250);
        assert_eq!(money("12.50"), 1_250);
        assert_eq!(money("12.500"), 1_250);
        assert_eq!(money("0.01"), 1);
    }

    #[test]
    fn money_with_separators_and_currency() {
        assert_eq!(money("1,200 บาท"), 120_000);
        assert_eq!(money("๑,๒๐๐.๕๐ บาท"), 120_050);
        assert_eq!(money("20บาท"), 2_000);
        assert_eq!(money("฿1,250"), 125_000);
        assert_eq!(money("฿ 20"), 2_000);
        assert_eq!(money("  12,345,678  "), 1_234_567_800);
    }

    #[test]
    fn money_from_zero_to_the_maximum_price() {
        assert_eq!(money("0"), 0);
        assert_eq!(money("๐"), 0);
        assert_eq!(money("100,000"), 10_000_000);
        assert_eq!(money("100000.00 บาท"), 10_000_000);
    }

    #[test]
    fn money_with_too_many_decimals_is_refused() {
        assert_eq!(parse_money("12.555"), Err(NumberError::TooManyDecimals));
        assert_eq!(parse_money("๑.๒๓๔"), Err(NumberError::TooManyDecimals));
    }

    #[test]
    fn whole_numbers() {
        assert_eq!(parse_whole("2"), Ok(2));
        assert_eq!(parse_whole("๑๐"), Ok(10));
        assert_eq!(parse_whole("99,999"), Ok(99_999));
        assert_eq!(parse_whole("2.0"), Ok(2));
        assert_eq!(parse_whole("0"), Ok(0));
    }

    #[test]
    fn fractions_are_not_whole() {
        assert_eq!(parse_whole("2.5"), Err(NumberError::NotWhole));
        assert_eq!(parse_whole("๒.๕"), Err(NumberError::NotWhole));
    }

    #[test]
    fn non_numbers_are_refused() {
        for text in [
            "",
            "   ",
            "บาท",
            "฿",
            "abc",
            "สิบ",
            "-5",
            "+5",
            "1,20",
            "1,2000",
            "1234,567",
            ",100",
            "100,",
            "1,,000",
            "1 200",
            "1.2.3",
            ".5",
            "12.",
            "12.x",
            "1บาท2",
        ] {
            assert_eq!(parse_money(text), Err(NumberError::NotANumber), "{text:?}");
            assert_eq!(parse_whole(text), Err(NumberError::NotANumber), "{text:?}");
        }
    }

    #[test]
    fn too_many_digits_are_refused() {
        let huge = "9".repeat(25);
        assert_eq!(parse_money(&huge), Err(NumberError::TooLarge));
        assert_eq!(parse_whole(&huge), Err(NumberError::TooLarge));
        assert_eq!(parse_whole("4,294,967,296"), Err(NumberError::TooLarge));
    }
}
