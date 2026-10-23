//! Button and Quick Reply labels short enough that LINE never truncates them (NFR-USE-04).

use std::borrow::Cow;

/// A button or Quick Reply label of at most [`Label::MAX_CHARS`] characters.
///
/// Characters are Unicode scalar values, so Thai vowel and tone marks count: `ที่` is 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label(Cow<'static, str>);

impl Label {
    pub const MAX_CHARS: usize = 20;

    /// A fixed label. Too long a text fails to compile in a `static` or `const`, as the
    /// template labels are.
    pub const fn from_static(text: &'static str) -> Self {
        assert!(
            char_count(text) <= Self::MAX_CHARS,
            "label longer than 20 characters"
        );
        Self(Cow::Borrowed(text))
    }

    /// A label built at run time (a zone, a category, `ใส่ 2 แผง`), shortened to fit with
    /// a trailing `…` when too long. The cut keeps each Thai consonant together with its
    /// vowels and tone marks.
    pub fn fit(text: impl Into<String>) -> Self {
        let text = text.into();
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= Self::MAX_CHARS {
            return Self(Cow::Owned(text));
        }
        // Room for the ellipsis, then back off to the start of a syllable
        let mut cut = Self::MAX_CHARS - 1;
        while cut > 0 && (belongs_to_previous(chars[cut]) || belongs_to_next(chars[cut - 1])) {
            cut -= 1;
        }
        let kept: String = chars[..cut].iter().collect();
        Self(Cow::Owned(format!("{}…", kept.trim_end())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Thai signs written above, below or after the consonant they follow: ั ำ ิ ี ึ ื ุ ู ฺ and
/// ็ ่ ้ ๊ ๋ ์ ํ ๎. Together with it they form one grapheme.
fn belongs_to_previous(c: char) -> bool {
    matches!(c, '\u{0E31}' | '\u{0E33}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}')
}

/// Thai vowels written before the consonant they belong to: เ แ โ ใ ไ.
fn belongs_to_next(c: char) -> bool {
    matches!(c, '\u{0E40}'..='\u{0E44}')
}

/// `str::chars().count()`, which isn't `const`: counts every byte that starts a character.
const fn char_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] & 0b1100_0000 != 0b1000_0000 {
            count += 1;
        }
        i += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(label: &Label) -> usize {
        label.as_str().chars().count()
    }

    #[test]
    fn vowel_and_tone_marks_count() {
        assert_eq!(char_count("ที่"), 3);
        assert_eq!(char_count("ส่งข้อความถึงลูกค้า"), 19);
        assert_eq!(char_count("วิธีเปิดร้านและลงสินค้า"), 23);
    }

    #[test]
    fn short_labels_are_kept() {
        assert_eq!(Label::fit("ใส่ 2 แผง").as_str(), "ใส่ 2 แผง");
        let twenty = "ไม่จำกัด / ทำตามสั่ง";
        assert_eq!(Label::fit(twenty).as_str(), twenty);
    }

    #[test]
    fn long_labels_are_shortened_with_an_ellipsis() {
        let label = Label::fit("ผักสวนครัวปลอดสารพิษเก็บสด");
        assert_eq!(chars(&label), Label::MAX_CHARS);
        assert_eq!(label.as_str(), "ผักสวนครัวปลอดสารพิ…");
    }

    #[test]
    fn a_cut_never_splits_a_syllable() {
        // The 20th character is the vowel of คุ
        assert_eq!(
            Label::fit("ขนมไทยโบราณจากบ้านคุณยายสมศรี").as_str(),
            "ขนมไทยโบราณจากบ้าน…"
        );
        // The 20th and 19th are the tone mark and vowel of มู่
        assert_eq!(
            Label::fit("ร้านค้าทั้งหมดในหมู่บ้านของเรา").as_str(),
            "ร้านค้าทั้งหมดในห…"
        );
        // The 19th is the leading vowel of เก็บ
        assert_eq!(
            Label::fit("ผักสวนครัวปลอดสาร เก็บสด").as_str(),
            "ผักสวนครัวปลอดสาร…"
        );
    }

    #[test]
    fn plain_text_is_cut_at_nineteen() {
        let label = Label::fit("a".repeat(30));
        assert_eq!(label.as_str(), format!("{}…", "a".repeat(19)));
    }
}
