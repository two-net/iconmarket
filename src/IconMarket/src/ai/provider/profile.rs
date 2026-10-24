//! What a model accepts and how it is asked, worked out once from its name by its adapter's
//! rule table (FR-AIP-04). Changing model is a config change only (FR-AIP-03); a name no rule
//! knows gets the provider's current-generation profile.

use super::Capabilities;

/// Added to `ai.maxOutputTokens` for a model whose thinking counts against its output limit,
/// so that the setting caps the visible answer on every provider
pub const THINKING_ALLOWANCE: u32 = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub capabilities: Capabilities,
    /// Whether `ai.temperature` is sent; models that reason reject or ignore it
    pub temperature: bool,
    /// How the model is asked to keep its reasoning short; `None` sends nothing
    pub reasoning: Option<Reasoning>,
    /// Whether the model thinks at that setting
    pub thinks: bool,
}

/// The reasoning setting sent, as the provider spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reasoning {
    /// An effort or level, such as `low`
    Effort(&'static str),
    /// A number of thinking tokens
    Budget(u32),
}

impl Profile {
    /// The output limit to send for `visible` tokens of answer, when the provider counts the
    /// model's thinking against it
    pub fn output_limit(&self, visible: u32) -> u32 {
        if self.thinks {
            visible.saturating_add(THINKING_ALLOWANCE)
        } else {
            visible
        }
    }
}

/// The first two numbers in a model name: `claude-opus-4-6` and `gemini-2.5-pro` give (4, 6)
/// and (2, 5); a lone number, or one followed by a date, has minor 0. `None` without a number.
pub fn version(model: &str) -> Option<(u32, u32)> {
    let mut numbers = model
        .split(['-', '.'])
        .skip_while(|token| token.parse::<u32>().is_err());
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers
        .next()
        .filter(|token| token.len() <= 2)
        .and_then(|token| token.parse().ok())
        .unwrap_or(0);
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_model_names() {
        for (model, version_) in [
            ("claude-opus-4-6", Some((4, 6))),
            ("claude-sonnet-4-20250514", Some((4, 0))),
            ("claude-3-5-haiku-latest", Some((3, 5))),
            ("claude-opus-5", Some((5, 0))),
            ("gemini-2.5-flash-lite", Some((2, 5))),
            ("gemini-4-argon", Some((4, 0))),
            ("grok-4.20-0309-reasoning", Some((4, 20))),
            ("a-model", None),
        ] {
            assert_eq!(version(model), version_, "{model}");
        }
    }

    #[test]
    fn thinking_gets_an_allowance_on_top_of_the_answer() {
        let profile = Profile {
            capabilities: Capabilities::ALL,
            temperature: false,
            reasoning: Some(Reasoning::Effort("low")),
            thinks: true,
        };
        assert_eq!(profile.output_limit(1024), 1024 + THINKING_ALLOWANCE);
        let plain = Profile {
            thinks: false,
            ..profile
        };
        assert_eq!(plain.output_limit(1024), 1024);
    }
}
