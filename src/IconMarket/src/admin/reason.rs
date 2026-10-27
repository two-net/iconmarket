//! The reason an admin gives for a decision: turning a shop's application down (FR-ADM-02),
//! removing a product or suspending a shop (FR-ADM-03), and suspending or unsuspending a user
//! (FR-ADM-04). It is typed in a guided flow of one step, or picked from common ones, and kept
//! in the audit log (FR-ADM-08); whoever the decision affects is told it.

/// The longest reason, in characters, as for a report (FR-HELP-02)
pub const REASON_CHARS: usize = 300;

/// Why a reason wasn't taken; the step asks again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Nothing but spaces
    Empty,
    /// Longer than [`REASON_CHARS`], in characters
    TooLong(usize),
}

/// `text` as a reason: without spaces around it, and each run of spaces or lines within it
/// made one space.
pub fn reason(text: &str) -> Result<String, Refused> {
    let reason = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match reason.chars().count() {
        0 => Err(Refused::Empty),
        chars if chars > REASON_CHARS => Err(Refused::TooLong(REASON_CHARS)),
        _ => Ok(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_tidied_and_bounded() {
        assert_eq!(
            reason("  ไม่ได้อยู่ \n ในหมู่บ้าน ").as_deref(),
            Ok("ไม่ได้อยู่ ในหมู่บ้าน")
        );
        let longest = "ก".repeat(REASON_CHARS);
        assert_eq!(reason(&longest), Ok(longest.clone()));
        assert_eq!(
            reason(&format!("{longest}ก")),
            Err(Refused::TooLong(REASON_CHARS))
        );
        assert_eq!(reason(" \n "), Err(Refused::Empty));
    }
}
