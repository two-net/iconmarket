//! AI gateway (SRS §2.1, §3.3): every use of the AI goes through [`Gateway`], so business
//! logic never depends on which provider is active (CON-02). It offers one provider-neutral
//! interface (FR-AIP-02), turns off features the configured model can't serve (FR-AIP-04),
//! enforces the daily limits (FR-AI-09), validates what the model returns (FR-AI-05), retries
//! only what is worth retrying (FR-AI-10) and logs every call (FR-AI-16).
//!
//! Each provider is one adapter in [`provider`], so adding or changing one touches nothing
//! outside this module (NFR-MNT-02, FR-AIP-03): Claude, Gemini, GPT and Grok (FR-AIP-01), and
//! the `mock` provider, which needs no network and no key (FR-AIP-05).
//!
//! The [`assistant`] answers free text with the tools of Table 4.9-A, run as the user, and
//! proposes changes for them to confirm (FR-AI-01 – FR-AI-04).

#![allow(
    dead_code,
    reason = "the features that call the AI are built by later issues"
)]

pub mod assistant;
mod call_log;
mod gateway;
mod limits;
mod provider;
mod request;
mod schema;
#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;
mod untrusted;
mod validate;

use crate::config::AiFeatures;

pub use gateway::Gateway;
#[allow(
    unused_imports,
    reason = "the features that call the AI are built by later issues"
)]
pub use {
    gateway::{Call, Unavailable},
    provider::Capabilities,
    request::{Media, Message, Native, Output, Part, Role, Tool, ToolCall},
    schema::{Problem, Schema},
    untrusted::{DATA_RULE, as_data},
    validate::Answer,
};

/// What the AI is used for, each turned on by its `ai.features` key (§6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Free-text understanding (FR-AI-01)
    Assistant,
    /// Product drafts from a photo or text (FR-PROD-02)
    Listing,
    /// Checking product and shop text (FR-PROD-09)
    Moderation,
    /// Reading payment slips (FR-PAY-06)
    SlipReading,
    /// Voice messages (FR-AI-15)
    Voice,
    /// Search by photo (FR-SRCH-10)
    VisualSearch,
}

impl Feature {
    pub const ALL: [Self; 6] = [
        Self::Assistant,
        Self::Listing,
        Self::Moderation,
        Self::SlipReading,
        Self::Voice,
        Self::VisualSearch,
    ];

    /// As logged and stored in `ai_call_logs.feature`
    pub fn name(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Listing => "listing",
            Self::Moderation => "moderation",
            Self::SlipReading => "slip_reading",
            Self::Voice => "voice",
            Self::VisualSearch => "visual_search",
        }
    }

    /// Whether its `ai.features` key turns it on
    fn configured(self, features: &AiFeatures) -> bool {
        match self {
            Self::Assistant => features.assistant,
            Self::Listing => features.listing,
            Self::Moderation => features.moderation,
            Self::SlipReading => features.slip_reading,
            Self::Voice => features.voice,
            Self::VisualSearch => features.visual_search,
        }
    }

    /// What the model must accept for this feature to work (FR-AIP-04). Listing and
    /// moderation send images only when the model accepts them (FR-PROD-09).
    fn needs(self) -> Capabilities {
        let none = Capabilities::default();
        match self {
            Self::Assistant => Capabilities {
                tool_calling: true,
                ..none
            },
            Self::Listing | Self::Moderation => Capabilities {
                structured_output: true,
                ..none
            },
            Self::SlipReading | Self::VisualSearch => Capabilities {
                image_input: true,
                structured_output: true,
                ..none
            },
            Self::Voice => Capabilities {
                audio_input: true,
                ..none
            },
        }
    }
}
