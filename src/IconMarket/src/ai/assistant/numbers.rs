//! Numbers in words inside guided flows (FR-AI-13): when a step can't read the number typed,
//! such as *ยี่สิบห้าบาท* for a price or *สองแผง* for a quantity, the AI reads it, while
//! `ai.features.assistant` is on, and the step asks back whether that is what was meant, with a
//! Quick Reply that types it in digits. The value is always echoed back, and the tap is the
//! confirmation: the step takes it as if typed, and checks it as it checks any. Without the AI,
//! or without an answer, the step explains how to type the number, as ever.
//!
//! Only text with Thai letters and no digits is asked about, so typos the step explains are
//! not sent. What was typed is data (FR-AI-06), and the call is the user's (FR-AI-09).

use serde_json::json;
use tracing::debug;

use crate::ai::{self, Call, DATA_RULE, Feature, Gateway, Output, Schema};
use crate::dispatcher::Turn;
use crate::line::{Action, Message};
use crate::thai::{self, Label, Money};

const RULES: &str = "A villager typed a number in Thai words, maybe with a unit or a misspelling, \
such as ยี่สิบห้าบาท or สองแผง. Answer with the number in Arabic digits, with a dot before any \
satang, such as 25 or 12.50; or null if the text is not one number.";

/// The longest a number may be, in characters, as the AI answers it
const DIGITS: usize = 12;

/// What a step expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// An amount in baht: a price, a fee
    Money,
    /// A whole number of at least 1: a quantity, a stock
    Whole,
}

/// What the AI read a number in words as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Money(Money),
    Whole(u32),
}

impl Read {
    /// As the Quick Reply types it: digits that the step reads.
    fn typed(self) -> String {
        match self {
            Self::Money(money) => {
                let satang = money.satang();
                match satang % 100 {
                    0 => (satang / 100).to_string(),
                    rest => format!("{}.{rest:02}", satang / 100),
                }
            }
            Self::Whole(count) => count.to_string(),
        }
    }

    /// As the question shows it: `฿25`, or `2 แผง` of `unit`.
    fn shown(self, unit: Option<&str>) -> String {
        match (self, unit) {
            (Self::Money(money), _) => money.to_string(),
            (Self::Whole(count), Some(unit)) => format!("{count} {unit}"),
            (Self::Whole(count), None) => count.to_string(),
        }
    }
}

/// What to ask back when a step couldn't read `text` as the number it `expect`s: whether the
/// AI's reading of it, shown with `unit` if any, is what the user meant, and the Quick Reply that
/// types it. `None` if the AI isn't asked, or reads no number.
pub async fn ask_back(
    turn: &Turn<'_>,
    ai: &Gateway,
    text: &str,
    expect: Expect,
    unit: Option<&str>,
) -> Option<(Message, Action)> {
    let read = read(turn, ai, text, expect).await?;
    let shown = read.shown(unit);
    let question = turn.templates.number_read(&shown);
    Some((question, Action::message(Label::fit(shown), read.typed())))
}

/// What `text` says in words, as the AI reads it: a number as `expect`ed, if any.
async fn read(turn: &Turn<'_>, ai: &Gateway, text: &str, expect: Expect) -> Option<Read> {
    let text = text.trim();
    let thai = text.chars().any(|c| ('\u{0E01}'..='\u{0E2E}').contains(&c));
    if !thai
        || text
            .chars()
            .any(|c| c.is_ascii_digit() || ('๐'..='๙').contains(&c))
    {
        return None;
    }
    if !ai.is_enabled(Feature::Assistant) {
        return None;
    }
    let value = Schema::string()
        .max_length(DIGITS)
        .describe("the number in Arabic digits");
    let call = Call {
        feature: Feature::Assistant,
        user_id: Some(turn.actor.user.id),
        system: format!("{RULES}\n\n{DATA_RULE}"),
        messages: vec![ai::Message::user(ai::as_data(
            "number",
            &json!({ "text": text }),
        ))],
        output: Output::Json(Schema::object().optional("value", value)),
    };
    let answer = match ai.complete(call).await {
        Ok(ai::Answer::Json(answer)) => answer,
        Ok(_) => return None,
        Err(unavailable) => {
            debug!(?unavailable, "a number in words not read");
            return None;
        }
    };
    let digits = answer["value"].as_str()?;
    match expect {
        Expect::Money => thai::parse_money(digits)
            .ok()
            .filter(|money| *money > Money::ZERO)
            .map(Read::Money),
        Expect::Whole => thai::parse_whole(digits)
            .ok()
            .filter(|count| *count > 0)
            .map(Read::Whole),
    }
}
