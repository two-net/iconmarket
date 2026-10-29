//! The AI assistant (SRS §4.9), while `ai.features.assistant` is on: free text that reaches
//! routing's step 5 (FR-UI-12), in plain, colloquial or misspelled Thai, such as *มีไข่ไก่ขายไหม
//! เอาสัก 2 แผง* (Appendix A.1), is answered with the market's own cards.
//!
//! The AI is sent the assistant's rules, the date and time, the user's roles, the [tools] for
//! those roles and the last `ai.historyTurns` turns of the conversation (FR-AI-01). It calls read
//! tools as it likes and proposal tools for changes; each runs as the user, with their
//! permissions, and what it finds is shown as the system's cards (FR-AI-02), every price, stock
//! figure, total, order number and status from the database (FR-AI-04). The AI never changes
//! anything: a proposal is a pending action and a confirm button built from the database, and
//! nothing changes until the user taps it (FR-AI-03, FR-UI-13). What users wrote, the message
//! and the products' and shops' text the tools return, is data that can't change the rules,
//! roles or permissions, and no authorization depends on what the AI answers (FR-AI-06, AC-16).
//! Nothing personal is sent but what a tool needs, and the user is known by their internal ID
//! (FR-AI-08).
//!
//! A message takes a few rounds: the model, the tools it calls, then the model again with what
//! they found, at most [`MAX_ROUNDS`], none started after [`REPLY_WITHIN`] (NFR-PERF-03). It
//! counts once against the daily limits however many rounds it takes (FR-AI-09). LINE's loading
//! animation shows meanwhile ([`loading`], FR-LINE-08). Without an answer the user gets a short apology and
//! the menus, which work as ever (FR-AI-10, NFR-REL-06).
//!
//! In a guided flow, a number written in words is [read](numbers) (FR-AI-13).

mod memory;
pub mod numbers;
mod shown;
#[cfg(test)]
mod tests;
mod tools;

use std::time::Duration;

use sea_orm::DbErr;
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::{debug, info};

pub use self::memory::clear;
use self::shown::Shown;
use self::tools::Toolbox;
use crate::ai::{self, Answer, Call, DATA_RULE, Feature, Gateway, Output, Part, Role};
use crate::config::{Config, PoliteParticle};
use crate::dispatcher::Turn;
use crate::line::{self, Message};
use crate::users::access::{self, authorize};

/// The most rounds a message takes: the model, the tools it calls, and the model again
const MAX_ROUNDS: usize = 4;
/// No round starts after this long, so that a reply comes within NFR-PERF-03's 10 s
const REPLY_WITHIN: Duration = Duration::from_secs(10);
/// The longest the assistant's own text may be, in characters, cut there at a space: FR-AI-07
/// asks for about 300, and the rules ask for that
const TEXT_CHARS: usize = 400;

/// The assistant's rules (FR-AI-03, FR-AI-04, FR-AI-07, FR-AI-12, FR-AI-14). The market, the
/// polite particle, the date and the user's roles follow.
const RULES: &str = "You are the assistant of a LINE market where villagers buy from and sell \
to each other. Help them find products and shops, buy, and run their shops, with the tools you \
are given: never guess what is for sale, its price or stock, or anyone's orders.

- Reply in polite, simple Thai, in at most 300 characters. The system shows the cards of what \
the tools found under your reply, with prices, stock, totals, order numbers and statuses from \
the database: never state those yourself; point to the cards.
- Users write colloquial Thai, with misspellings and regional words: read them as a villager \
would. Search with the usual name of an item, then other words for it.
- You change nothing yourself. Tools named propose_ only offer a change, which the user confirms \
by tapping its button. Never say something was added, ordered, cancelled, paid or changed: say \
what tapping the button does.
- When the user wants an item, search for it, then propose putting the quantity they asked in \
the cart; if they named a shop, only that shop's.
- Draft a product only from what the seller wrote; never invent ingredients, sizes, origin or \
health claims.
- Answer how to use the market only from get_help. If no topic fits, say so and point to \
\"ช่วยเหลือ\".
- Decline what is not about the market in one short sentence, and say what you can help with.
- When a tool returns an error, say simply why and what the user can do.
- Earlier messages may include <shown>: what the user was shown then, by id, so that \"อันแรก\" \
or \"ร้านนั้น\" can be found.";

/// Answers `text`, typed by the actor at routing's step 5 (FR-UI-12), once routing started the
/// [`loading`] animation. `counted` if the message already counted against the daily limits, as
/// when the listing assistant read it first.
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    ai: &Gateway,
    text: &str,
    counted: bool,
) -> Result<Vec<Message>, DbErr> {
    // Suspended users can't use the AI (FR-ADM-04)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::UseAi).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let user_id = turn.actor.user.id;
    let toolbox = Toolbox::of(turn).await?;
    let mut messages = memory::recall(turn.db, user_id, config.ai.history_turns).await?;
    messages.push(memory::said(text));
    let system = system(turn, config);

    let started = Instant::now();
    let mut shown = Shown::default();
    let mut said = String::new();
    let mut answered = false;
    for round in 0..MAX_ROUNDS {
        let call = Call {
            feature: Feature::Assistant,
            user_id: Some(user_id),
            system: system.clone(),
            messages: messages.clone(),
            output: Output::Tools(toolbox.tools()),
        };
        // Counted once a message (FR-AI-09)
        let result = if round == 0 && !counted {
            ai.complete(call).await
        } else {
            ai.follow_up(call).await
        };
        let answer = match result {
            Ok(answer) => answer,
            Err(unavailable) if !answered => {
                return Ok(vec![unavailable.message(turn.templates)]);
            }
            // What the tools found is shown all the same
            Err(unavailable) => {
                info!(
                    ?unavailable,
                    round, "the assistant stopped after its tools ran"
                );
                break;
            }
        };
        answered = true;
        let Answer::Tools { text, calls, .. } = &answer else {
            // Never: what offers tools is answered with text or calls (FR-AI-05)
            break;
        };
        if !text.is_empty() {
            said.clone_from(text);
        }
        if calls.is_empty() {
            break;
        }
        messages.push(answer.assistant_turn());
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let content = if shown.started() {
                // A flow's question ends the reply
                Value::String(ai::as_data(&call.name, &json!({ "error": "not_run" })))
            } else {
                tools::run(turn, config, ai, &toolbox, call, &mut shown).await?
            };
            results.push(Part::ToolResult {
                call_id: call.id.clone(),
                content,
            });
        }
        messages.push(ai::Message {
            role: Role::Tool,
            parts: results,
        });
        if shown.started() || started.elapsed() >= REPLY_WITHIN {
            break;
        }
    }
    debug!(
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "the assistant answered"
    );

    let said = cut(said.trim(), TEXT_CHARS);
    if said.is_empty() && shown.is_empty() {
        return Ok(vec![turn.templates.ai_unavailable()]);
    }
    memory::remember(turn.db, user_id, turn.now, text, &shown.memory(), &said).await?;
    let text = if !said.is_empty() {
        Some(Message::text(said))
    } else if shown.started() {
        // The flow's question says what comes next
        None
    } else {
        Some(turn.templates.assistant_found())
    };
    shown.render(turn, config, text).await
}

/// Starts LINE's loading animation in the user's chat while the AI reads their message
/// (FR-LINE-08), until the reply comes. Only for show: if it can't be started, the reply comes
/// all the same.
pub async fn loading(turn: &Turn<'_>, config: &Config, line: &line::Client) {
    let Some(chat_id) = turn.actor.user.line_user_id.as_deref() else {
        return;
    };
    let seconds = config.ai.timeout_seconds.saturating_add(10);
    if let Err(e) = line.start_loading(chat_id, seconds).await {
        debug!(error = %e, "no loading animation");
    }
}

/// The system instructions: the rules, then who and when (FR-AI-01), then how data is given
/// (FR-AI-06).
fn system(turn: &Turn<'_>, config: &Config) -> String {
    let particle = match config.app.polite_particle {
        PoliteParticle::Kha => "ค่ะ",
        PoliteParticle::Khrap => "ครับ",
    };
    let roles = turn.actor.roles();
    let mut who = vec!["a buyer"];
    if roles.seller {
        who.push("a seller, who runs their own shop");
    }
    if roles.admin {
        who.push("an admin, who may only read through you");
    }
    format!(
        "{RULES}\n\nThe market is {app} of {village}. End sentences with {particle}. Now it is \
         {now}. The user is {who}.\n\n{DATA_RULE}",
        app = config.app.name,
        village = config.village.name,
        now = turn.templates.date_time(turn.now),
        who = who.join(" and "),
    )
}

/// `text`, cut at the last space before `chars` characters if it is longer (FR-AI-07).
fn cut(text: &str, chars: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(chars) else {
        return text.to_owned();
    };
    let head = &text[..end];
    let head = head
        .rfind(char::is_whitespace)
        .map_or(head, |space| &head[..space]);
    format!("{}…", head.trim_end())
}
