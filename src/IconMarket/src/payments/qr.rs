//! The PromptPay QR code for an order's exact amount (FR-PAY-04), made here to the Thai QR
//! payment standard (§1.5 ref. 5), without any outside service.
//!
//! The standard is EMVCo's merchant-presented QR: fields of a two-digit ID, a two-digit length
//! and the value, ending with a CRC of all that comes before. The fields go in the order the
//! payloads that Thai banking apps have long scanned put them, `promptpay-qr`'s:
//!
//! | ID | Field | Value |
//! | --- | --- | --- |
//! | `00` | Payload format | `01` |
//! | `01` | Point of initiation | `12` for one amount, used once; `11` for any |
//! | `29` | PromptPay | the payee, as fields of its own |
//! | `58` | Country | `TH` |
//! | `53` | Currency | `764`, the baht |
//! | `54` | Amount | `230.00`, with an amount only |
//! | `63` | CRC | CRC-16/CCITT-FALSE of all before it, `6304` included, in upper-case hex |
//!
//! Within field 29, `00` is PromptPay's application ID, then the payee's PromptPay ID by its
//! kind: `01` a mobile number, written as an international one, `02` a citizen or tax ID, or
//! `03` an e-wallet ID.

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, GrayImage, ImageEncoder, Luma};
use qrcode::{Color, EcLevel, QrCode};

use crate::thai::Money;

/// PromptPay's application ID, under which field 29 names the payee
const PROMPTPAY_AID: &str = "A000000677010111";
/// Pixels per module: the codes drawn have 29 to 33 modules, so they come to 370–410 px with
/// the quiet zone, enough to scan from a screen or a saved photo
const MODULE_PX: u32 = 10;
/// The light margin the QR standard asks for around a code, in modules
const QUIET_ZONE: u32 = 4;

/// The payload of the PromptPay QR code paying `amount` to PromptPay ID `id`, its digits as a
/// shop gives it ([`parse_promptpay_id`](super::parse_promptpay_id)): a mobile number of 10
/// digits, a citizen or tax ID of 13, or an e-wallet ID of 15. Without an amount, the payer
/// types one. `None` for anything else, or an amount that isn't positive.
pub fn promptpay_payload(id: &str, amount: Option<Money>) -> Option<String> {
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let payee = match id.len() {
        // As an international number, zero-padded to 13 digits: `0066812345678`
        10 => field("01", &format!("{:0>13}", format!("66{}", &id[1..]))),
        13 => field("02", id),
        15 => field("03", id),
        _ => return None,
    };
    let amount = match amount {
        Some(amount) if amount.satang() <= 0 => return None,
        Some(amount) => {
            let satang = amount.satang();
            Some(format!("{}.{:02}", satang / 100, satang % 100))
        }
        None => None,
    };

    let mut payload = field("00", "01");
    payload += &field("01", if amount.is_some() { "12" } else { "11" });
    payload += &field("29", &(field("00", PROMPTPAY_AID) + &payee));
    payload += &field("58", "TH");
    payload += &field("53", "764");
    if let Some(amount) = amount {
        payload += &field("54", &amount);
    }
    // The CRC covers its own ID and length
    payload += "6304";
    let crc = crc16(payload.as_bytes());
    Some(format!("{payload}{crc:04X}"))
}

/// A QR code drawn as a PNG: dark modules on white, `side` pixels square.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrImage {
    pub png: Vec<u8>,
    pub side: u32,
}

/// `payload` drawn as a QR code, with medium error correction.
pub fn draw_qr(payload: &str) -> QrImage {
    let code = QrCode::with_error_correction_level(payload, EcLevel::M)
        .expect("a payload of about 90 characters fits a QR code");
    let modules = u32::try_from(code.width()).expect("at most 177 modules");
    let colors = code.to_colors();
    let side = (modules + 2 * QUIET_ZONE) * MODULE_PX;
    let image = GrayImage::from_fn(side, side, |x, y| {
        let (column, row) = (x / MODULE_PX, y / MODULE_PX);
        let inside = QUIET_ZONE..QUIET_ZONE + modules;
        let dark = inside.contains(&column)
            && inside.contains(&row)
            && colors[usize::try_from((row - QUIET_ZONE) * modules + column - QUIET_ZONE)
                .expect("a module of the code")]
                == Color::Dark;
        Luma([if dark { 0 } else { 255 }])
    });
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(image.as_raw(), side, side, ExtendedColorType::L8)
        .expect("encoding into memory never fails");
    QrImage { png, side }
}

/// One field: its ID, the length of `value` in two digits, and `value`.
fn field(id: &str, value: &str) -> String {
    debug_assert!(value.len() < 100, "a field of at most 99 characters");
    format!("{id}{:02}{value}", value.len())
}

/// CRC-16/CCITT-FALSE: polynomial 0x1021, starting from 0xFFFF, not reflected.
fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0xFFFF, |crc, &byte| {
        (0..8).fold(crc ^ (u16::from(byte) << 8), |crc, _| {
            if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            }
        })
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The text of the QR code drawn as `png`, as a banking app scanning it reads it.
    pub fn scan_qr(png: &[u8]) -> String {
        let image = image::load_from_memory(png).expect("a PNG").into_luma8();
        let (width, height) = image.dimensions();
        let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(
            usize::try_from(width).unwrap(),
            usize::try_from(height).unwrap(),
            |x, y| image.get_pixel(u32::try_from(x).unwrap(), u32::try_from(y).unwrap())[0],
        );
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1, "one QR code");
        grids[0].decode().expect("a readable QR code").1
    }

    #[test]
    fn the_crc_is_ccitt_false() {
        // The standard check value of CRC-16/CCITT-FALSE
        assert_eq!(crc16(b"123456789"), 0x29B1);
        assert_eq!(crc16(b""), 0xFFFF);
    }

    /// The payloads `promptpay-qr` makes for the same IDs and amount, which Thai banking apps
    /// scan
    #[test]
    fn payloads_match_those_banking_apps_scan() {
        for (id, amount, expected) in [
            (
                "0801234567",
                None,
                "00020101021129370016A000000677010111011300668012345675802TH530376463046197",
            ),
            (
                "1111111111111",
                None,
                "00020101021129370016A000000677010111021311111111111115802TH530376463047B5A",
            ),
            (
                "012345678901234",
                None,
                "00020101021129390016A00000067701011103150123456789012345802TH530376463049781",
            ),
            (
                "0000000000",
                Some(Money::from_satang(422)),
                "00020101021229370016A000000677010111011300660000000005802TH530376454044.226304E469",
            ),
        ] {
            assert_eq!(
                promptpay_payload(id, amount).as_deref(),
                Some(expected),
                "{id}"
            );
        }
    }

    #[test]
    fn the_amount_is_exact_to_the_satang() {
        let at = |amount| promptpay_payload("0812345678", Some(amount)).unwrap();
        assert!(at(Money::from_baht(230)).contains("5406230.00"));
        assert!(at(Money::from_satang(1_250)).contains("540512.50"));
        assert!(at(Money::from_satang(1)).contains("54040.01"));
        assert!(at(Money::from_baht(1_250)).contains("54071250.00"));
        // An amount makes the code one for that amount, used once
        assert!(at(Money::from_baht(230)).starts_with("000201010212"));
    }

    /// What the buyer of a ฿230 order from a shop taking PromptPay on 081-234-5678 scans
    #[test]
    fn a_mobile_is_written_as_an_international_number() {
        let mobile = promptpay_payload("0812345678", Some(Money::from_baht(230))).unwrap();
        assert_eq!(
            mobile,
            "00020101021229370016A000000677010111011300668123456785802TH53037645406230.0063042701"
        );
    }

    #[test]
    fn anything_else_has_no_payload() {
        let amount = Some(Money::from_baht(230));
        for id in [
            "",
            "081234567",
            "08123456789",
            "110170020345",
            "11017002034500",
            "0812345678901234",
            "081-234-5678",
            "๐๘๑๒๓๔๕๖๗๘",
        ] {
            assert_eq!(promptpay_payload(id, amount), None, "{id:?}");
        }
        assert_eq!(promptpay_payload("0812345678", Some(Money::ZERO)), None);
        assert_eq!(
            promptpay_payload("0812345678", Some(Money::from_satang(-100))),
            None
        );
    }

    #[test]
    fn the_code_drawn_scans_as_its_payload() {
        for (id, amount) in [
            ("0812345678", Money::from_baht(230)),
            ("1101700203450", Money::from_satang(1_250)),
            ("004999012345678", Money::from_satang(9_999_999)),
        ] {
            let payload = promptpay_payload(id, Some(amount)).unwrap();
            let drawn = draw_qr(&payload);
            assert_eq!(scan_qr(&drawn.png), payload, "{id}");
            let image = image::load_from_memory(&drawn.png).unwrap();
            assert_eq!((image.width(), image.height()), (drawn.side, drawn.side));
            assert!((370..=410).contains(&drawn.side), "{id}: {} px", drawn.side);
        }
    }
}
