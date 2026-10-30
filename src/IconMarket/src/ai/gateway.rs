//! The gateway around the adapters: features turned off when the model can't serve them
//! (FR-AIP-04), the daily limits (FR-AI-09), the timeout and retries (FR-AI-10), validation
//! (FR-AI-05), the fallback provider (FR-AIP-06) and logging (FR-AI-16).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use chrono_tz::Tz;
use sea_orm::DatabaseConnection;
use sqlx::PgPool;
use tokio::time::Instant;
use tracing::{info, warn};

use super::Feature;
use super::call_log::{self, Outcome};
use super::limits;
use super::provider::{Adapter, Capabilities};
use super::request::{Message, Output, Part, Request, Usage};
use super::validate::{self, Answer};
use crate::config::{AiConfig, AiEndpoint};
use crate::line;
use crate::notifications::templates::Templates;
use crate::persistence;

/// Waited before the one retry of a transient error
const RETRY_WAIT: Duration = Duration::from_secs(1);

/// The one way to the AI (FR-AIP-02). Cheap to clone: clones share everything.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None` when `ai.enabled` is off or the provider has no adapter
    adapter: Option<Adapter>,
    /// Asked when `adapter` leaves a call unanswered (FR-AIP-06); never without it
    fallback: Option<Adapter>,
    /// The features `ai.features` turns on that the model can serve
    enabled: Vec<Feature>,
    timeout: Duration,
    retry_wait: Duration,
    max_output_tokens: u32,
    temperature: f64,
    per_user_daily_calls: u32,
    global_daily_calls: u32,
    log_content: bool,
    time_zone: Tz,
    db: DatabaseConnection,
}

/// One use of the AI.
#[derive(Clone, Debug)]
pub struct Call {
    pub feature: Feature,
    /// The internal user ID, never the LINE one (FR-AI-08); `None` for a call not made for a
    /// user, which counts against the global limit only
    pub user_id: Option<i64>,
    pub system: String,
    pub messages: Vec<Message>,
    pub output: Output,
}

/// Why there is no answer. The caller shows [`Unavailable::message`] or carries on without
/// the AI; either way the flows that don't need it keep working (FR-AI-10, NFR-REL-06).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The feature is off: by config, or because the model can't serve it (FR-AIP-04)
    Disabled,
    /// A daily limit is reached, so the provider wasn't called (FR-AI-09)
    Resting,
    /// The provider, and any fallback that could take the call, failed, timed out or answered
    /// invalidly twice (FR-AI-05, FR-AI-10, FR-AIP-06)
    Failed,
}

impl Unavailable {
    /// What to tell the user, with the menus to carry on from.
    pub fn message(self, templates: &Templates) -> line::Message {
        match self {
            Self::Resting => templates.ai_resting(),
            Self::Disabled | Self::Failed => templates.ai_unavailable(),
        }
    }
}

/// What to do after one try.
enum Next {
    Done(Answer),
    /// Again, after a transient error
    Retry,
    /// Again, telling the model what was wrong with its answer
    Hint(Vec<String>),
    Fail,
}

impl Gateway {
    /// The gateway for the configured provider. Logs which features are on, and warns about
    /// each one the model can't serve (FR-AIP-04). A provider without an adapter turns the AI
    /// off rather than stopping startup, as nothing else needs it (NFR-REL-06).
    pub fn new(config: &AiConfig, time_zone: Tz, db: PgPool) -> Self {
        let adapter = match &config.primary {
            None => {
                info!("AI is off: ai.enabled is false");
                None
            }
            Some(endpoint) => match Adapter::new(endpoint, config.base_url.as_ref()) {
                Ok(adapter) => Some(adapter),
                Err(reason) => {
                    warn!(
                        provider = endpoint.provider.name(),
                        model = %endpoint.model,
                        reason = %reason,
                        "AI is off: the provider has no adapter"
                    );
                    None
                }
            },
        };
        let fallback = match (&adapter, &config.fallback) {
            (Some(_), Some(endpoint)) => fallback_adapter(endpoint),
            _ => None,
        };
        let timeout = Duration::from_secs(config.timeout_seconds.into());
        Self::build(
            config, adapter, fallback, time_zone, db, timeout, RETRY_WAIT,
        )
    }

    /// A gateway for `adapter` that gives up after `timeout` and doesn't wait to retry.
    #[cfg(test)]
    pub fn for_test(
        config: &AiConfig,
        adapter: Option<Adapter>,
        db: PgPool,
        timeout: Duration,
    ) -> Self {
        let time_zone = chrono_tz::Asia::Bangkok;
        Self::build(
            config,
            adapter,
            None,
            time_zone,
            db,
            timeout,
            Duration::ZERO,
        )
    }

    /// As [`Gateway::for_test`], asking `fallback` when `adapter` leaves a call unanswered.
    #[cfg(test)]
    pub fn with_fallback_for_test(
        config: &AiConfig,
        adapter: Adapter,
        fallback: Adapter,
        db: PgPool,
        timeout: Duration,
    ) -> Self {
        let time_zone = chrono_tz::Asia::Bangkok;
        let (adapter, fallback) = (Some(adapter), Some(fallback));
        Self::build(
            config,
            adapter,
            fallback,
            time_zone,
            db,
            timeout,
            Duration::ZERO,
        )
    }

    fn build(
        config: &AiConfig,
        adapter: Option<Adapter>,
        fallback: Option<Adapter>,
        time_zone: Tz,
        db: PgPool,
        timeout: Duration,
        retry_wait: Duration,
    ) -> Self {
        let enabled = adapter.as_ref().map_or_else(Vec::new, |adapter| {
            enabled_features(config, adapter, fallback.as_ref())
        });
        Self {
            inner: Arc::new(Inner {
                adapter,
                fallback,
                enabled,
                timeout,
                retry_wait,
                max_output_tokens: config.max_output_tokens,
                temperature: config.temperature,
                per_user_daily_calls: config.limits.per_user_daily_calls,
                global_daily_calls: config.limits.global_daily_calls,
                log_content: config.log_content,
                time_zone,
                db: persistence::orm(db),
            }),
        }
    }

    /// Whether `feature` can be used: AI is on, `ai.features` turns it on and the model can
    /// serve it. Check before offering a feature, e.g. when routing an image (FR-UI-14).
    pub fn is_enabled(&self, feature: Feature) -> bool {
        self.inner.enabled.contains(&feature)
    }

    /// Whether the model accepts images, for the features that send them only then, such as
    /// moderation (FR-PROD-09). The adapters send every image they are given. A fallback
    /// without images doesn't take those calls, so it costs healthy calls nothing.
    pub fn accepts_images(&self) -> bool {
        self.inner
            .adapter
            .as_ref()
            .is_some_and(|adapter| adapter.capabilities().image_input)
    }

    /// Asks the model, and returns its answer once it is valid.
    ///
    /// The call counts once against the daily limits; at a limit the provider isn't called
    /// (FR-AI-09). A rate limit or server error is tried again once, a timeout never
    /// (FR-AI-10). An invalid answer is tried again once with a hint naming its problems
    /// (FR-AI-05). A call still unanswered goes to the fallback, if it can take it, by the same
    /// rules (FR-AIP-06). Every try is logged, with the provider it went to (FR-AI-16).
    pub async fn complete(&self, call: Call) -> Result<Answer, Unavailable> {
        self.ask(call, true).await
    }

    /// Asks the model again for a use already counted, as [`Gateway::complete`] does but
    /// without counting against the daily limits: the assistant's rounds after its tools ran,
    /// so that a user's message counts once however many rounds it takes (FR-AI-09). Still
    /// logged, try by try (FR-AI-16).
    pub async fn follow_up(&self, call: Call) -> Result<Answer, Unavailable> {
        self.ask(call, false).await
    }

    async fn ask(&self, call: Call, counted: bool) -> Result<Answer, Unavailable> {
        let inner = &*self.inner;
        let Call {
            feature,
            user_id,
            system,
            messages,
            output,
        } = call;
        let adapter = match &inner.adapter {
            Some(adapter) if self.is_enabled(feature) => adapter,
            _ => return Err(Unavailable::Disabled),
        };

        if counted {
            let keys = limits::keys(Utc::now(), inner.time_zone, user_id);
            let reserved = limits::reserve(
                &inner.db,
                keys,
                inner.global_daily_calls,
                inner.per_user_daily_calls,
            );
            match reserved.await {
                Ok(true) => {}
                Ok(false) => {
                    info!(
                        feature = feature.name(),
                        "AI daily limit reached, the assistant is resting"
                    );
                    return Err(Unavailable::Resting);
                }
                Err(e) => {
                    warn!(error = %e, feature = feature.name(), "cannot count the AI call");
                    return Err(Unavailable::Failed);
                }
            }
        }

        let mut request = Request {
            system,
            messages,
            output,
            max_output_tokens: inner.max_output_tokens,
            temperature: inner.temperature,
            timeout: inner.timeout,
        };
        let asked = request.messages.len();
        if let Some(answer) = self.tries(adapter, feature, user_id, &mut request).await {
            return Ok(answer);
        }

        let Some(fallback) = &inner.fallback else {
            return Err(Unavailable::Failed);
        };
        if !can_take(fallback, feature, &request) {
            info!(
                feature = feature.name(),
                provider = fallback.provider(),
                model = fallback.model(),
                "the AI fallback can't take this call"
            );
            return Err(Unavailable::Failed);
        }
        info!(
            feature = feature.name(),
            provider = fallback.provider(),
            model = fallback.model(),
            "trying the AI fallback"
        );
        // A hint is about an answer the fallback never gave (FR-AI-05)
        request.messages.truncate(asked);
        let answer = self.tries(fallback, feature, user_id, &mut request).await;
        answer.ok_or(Unavailable::Failed)
    }

    /// Asks `adapter` until it answers validly or gives up, as [`Gateway::complete`] says.
    /// `request` keeps any hint it was given.
    async fn tries(
        &self,
        adapter: &Adapter,
        feature: Feature,
        user_id: Option<i64>,
        request: &mut Request,
    ) -> Option<Answer> {
        let inner = &*self.inner;
        let (mut retried, mut hinted) = (false, false);
        loop {
            let started = Instant::now();
            let result = tokio::time::timeout(inner.timeout, adapter.complete(request)).await;
            let latency = started.elapsed();
            if inner.log_content {
                let response = result.as_ref().ok().and_then(|result| result.as_ref().ok());
                call_log::content(feature, request, response);
            }

            let (outcome, usage, detail, next) = match result {
                // Not this provider again: the user has waited long enough (FR-AI-10)
                Err(_) => {
                    let detail = format!("no answer within {:?}", inner.timeout);
                    (Outcome::Timeout, Usage::default(), Some(detail), Next::Fail)
                }
                Ok(Err(e)) => {
                    let next = if e.is_transient() && !retried {
                        Next::Retry
                    } else {
                        Next::Fail
                    };
                    (e.outcome(), Usage::default(), Some(e.to_string()), next)
                }
                Ok(Ok(response)) => match validate::check(&request.output, &response) {
                    Ok(answer) => (Outcome::Ok, response.usage, None, Next::Done(answer)),
                    Err(problems) => {
                        let detail = problems.join("; ");
                        let next = if hinted {
                            Next::Fail
                        } else {
                            Next::Hint(problems)
                        };
                        (Outcome::Invalid, response.usage, Some(detail), next)
                    }
                },
            };
            let logged = call_log::Call {
                feature,
                user_id,
                provider: adapter.provider(),
                model: adapter.model(),
                usage,
                latency,
                outcome,
                detail,
            };
            call_log::record(&inner.db, logged).await;

            match next {
                Next::Done(answer) => return Some(answer),
                Next::Fail => return None,
                Next::Retry => {
                    retried = true;
                    tokio::time::sleep(inner.retry_wait).await;
                }
                Next::Hint(problems) => {
                    hinted = true;
                    request.messages.push(validate::hint(&problems));
                }
            }
        }
    }
}

/// The adapter for `ai.fallback`, or `None` with a warning if there can't be one: the primary
/// works without it. It calls its provider's own API, as `ai.baseUrl` is written for the
/// primary's and would be sent the fallback's key (CFG-06).
fn fallback_adapter(endpoint: &AiEndpoint) -> Option<Adapter> {
    match Adapter::new(endpoint, None) {
        Ok(adapter) => Some(adapter),
        Err(reason) => {
            warn!(
                provider = endpoint.provider.name(),
                model = %endpoint.model,
                reason = %reason,
                "AI fallback is off: the provider has no adapter"
            );
            None
        }
    }
}

/// Whether `adapter` can serve `request`. Features follow the primary (FR-AIP-04), so a fallback
/// may lack what the feature needs or what this call sends, and adapters would leave out a clip
/// or send an image the model can't read.
fn can_take(adapter: &Adapter, feature: Feature, request: &Request) -> bool {
    let sends = |kind: fn(&Part) -> bool| {
        let mut parts = request.messages.iter().flat_map(|message| &message.parts);
        parts.any(kind)
    };
    let needed = feature.needs();
    let needed = Capabilities {
        image_input: needed.image_input || sends(|part| matches!(part, Part::Image(_))),
        audio_input: needed.audio_input || sends(|part| matches!(part, Part::Audio(_))),
        ..needed
    };
    adapter.capabilities().missing(needed).is_empty()
}

/// The features `ai.features` turns on that the model can serve. Each one it can't is turned
/// off with a warning (FR-AIP-04); a fallback turns none on, and is warned about too.
fn enabled_features(
    config: &AiConfig,
    adapter: &Adapter,
    fallback: Option<&Adapter>,
) -> Vec<Feature> {
    let capabilities = adapter.capabilities();
    let enabled: Vec<Feature> = Feature::ALL
        .into_iter()
        .filter(|feature| feature.configured(&config.features))
        .filter(|feature| {
            let missing = capabilities.missing(feature.needs());
            if !missing.is_empty() {
                warn!(
                    feature = feature.name(),
                    missing = missing.join(", "),
                    provider = adapter.provider(),
                    model = adapter.model(),
                    "AI feature turned off: the model lacks what it needs"
                );
            }
            missing.is_empty()
        })
        .collect();
    let names: Vec<&str> = enabled.iter().map(|feature| feature.name()).collect();
    info!(
        provider = adapter.provider(),
        model = adapter.model(),
        fallback_provider = fallback.map(Adapter::provider),
        fallback_model = fallback.map(Adapter::model),
        features = names.join(", "),
        "AI gateway ready"
    );
    if let Some(fallback) = fallback {
        for feature in &enabled {
            let missing = fallback.capabilities().missing(feature.needs());
            if !missing.is_empty() {
                warn!(
                    feature = feature.name(),
                    missing = missing.join(", "),
                    provider = fallback.provider(),
                    model = fallback.model(),
                    "the AI fallback can't serve this feature, so it has none"
                );
            }
        }
    }
    enabled
}

#[cfg(test)]
mod tests {
    use chrono_tz::Asia::Bangkok;

    use super::*;
    use crate::ai::provider::{Capabilities, Mock};
    use crate::config::{AiEndpoint, AiFeatures, AiLimits, AiProvider, Secret};
    use crate::persistence::testing;

    fn config(primary: Option<AiEndpoint>) -> AiConfig {
        AiConfig {
            primary,
            base_url: None,
            timeout_seconds: 20,
            max_output_tokens: 1024,
            temperature: 0.3,
            history_turns: 10,
            features: AiFeatures {
                assistant: true,
                listing: true,
                moderation: true,
                slip_reading: true,
                voice: true,
                visual_search: true,
            },
            limits: AiLimits {
                per_user_daily_calls: 50,
                global_daily_calls: 2000,
            },
            log_content: false,
            fallback: None,
        }
    }

    fn endpoint(provider: AiProvider) -> AiEndpoint {
        AiEndpoint {
            provider,
            model: "a-model".to_owned(),
            api_key: Some(Secret::new("ai-key-value")),
        }
    }

    fn enabled(gateway: &Gateway) -> Vec<Feature> {
        Feature::ALL
            .into_iter()
            .filter(|feature| gateway.is_enabled(*feature))
            .collect()
    }

    fn ask(feature: Feature) -> Call {
        Call {
            feature,
            user_id: None,
            system: "rules".to_owned(),
            messages: vec![Message::user("สวัสดี")],
            output: Output::Text,
        }
    }

    /// FR-AIP-04: a model without image or audio input can't read slips, search by photo or
    /// hear voice messages, and is never asked to.
    #[tokio::test]
    async fn features_the_model_cannot_serve_are_turned_off() {
        let text_only = Capabilities {
            tool_calling: true,
            structured_output: true,
            ..Capabilities::default()
        };
        let mock = Mock::scripted([]).with_capabilities(text_only);
        let adapter = Adapter::Mock(mock.clone());
        let gateway = Gateway::for_test(
            &config(None),
            Some(adapter),
            testing::unreachable(),
            Duration::from_secs(1),
        );
        assert_eq!(
            enabled(&gateway),
            [Feature::Assistant, Feature::Listing, Feature::Moderation]
        );
        for feature in [Feature::SlipReading, Feature::VisualSearch, Feature::Voice] {
            assert_eq!(
                gateway.complete(ask(feature)).await,
                Err(Unavailable::Disabled)
            );
        }
        assert!(mock.requests().is_empty());
    }

    /// FR-AIP-04: what was heard comes back as JSON, so a model that takes audio but can't
    /// answer in it can't hear voice messages either.
    #[tokio::test]
    async fn voice_messages_need_structured_output_too() {
        let audio_only = Capabilities {
            audio_input: true,
            ..Capabilities::default()
        };
        let adapter = Adapter::Mock(Mock::scripted([]).with_capabilities(audio_only));
        let gateway = Gateway::for_test(
            &config(None),
            Some(adapter),
            testing::unreachable(),
            Duration::from_secs(1),
        );
        assert!(!gateway.is_enabled(Feature::Voice));
    }

    fn with_fallback(config: &AiConfig, primary: &Mock, fallback: &Mock) -> Gateway {
        Gateway::with_fallback_for_test(
            config,
            Adapter::Mock(primary.clone()),
            Adapter::Mock(fallback.clone()),
            testing::unreachable(),
            Duration::from_secs(1),
        )
    }

    /// FR-AIP-04, FR-AIP-06: a feature the primary can't serve stays off, as the fallback is
    /// asked only after the primary.
    #[tokio::test]
    async fn the_fallback_turns_no_feature_on() {
        let audio = Capabilities {
            audio_input: false,
            ..Capabilities::ALL
        };
        let primary = Mock::scripted([]).with_capabilities(audio);
        let gateway = with_fallback(&config(None), &primary, &Mock::scripted([]));
        assert!(!gateway.is_enabled(Feature::Voice));
        assert!(gateway.is_enabled(Feature::SlipReading));
    }

    /// Whether photos are sent follows the primary alone, whatever the fallback accepts.
    #[tokio::test]
    async fn only_the_primary_decides_whether_photos_are_sent() {
        let no_images = Capabilities {
            image_input: false,
            ..Capabilities::ALL
        };
        let without = Mock::scripted([]).with_capabilities(no_images);
        let with = Mock::scripted([]);
        assert!(with_fallback(&config(None), &with, &without).accepts_images());
        assert!(!with_fallback(&config(None), &without, &with).accepts_images());
    }

    /// CFG-06: `ai.baseUrl` is the primary's, and the fallback's key never goes there.
    #[tokio::test]
    async fn the_fallback_calls_its_own_api() {
        let mut config = config(Some(endpoint(AiProvider::Claude)));
        config.base_url = Some("https://proxy.example.com/".parse().unwrap());
        config.fallback = Some(endpoint(AiProvider::Gemini));

        let gateway = Gateway::new(&config, Bangkok, testing::unreachable());

        let primary = format!("{:?}", gateway.inner.adapter.as_ref().unwrap());
        let fallback = format!("{:?}", gateway.inner.fallback.as_ref().unwrap());
        assert!(primary.contains("proxy.example.com"), "{primary}");
        assert!(!fallback.contains("proxy.example.com"), "{fallback}");
        assert!(
            fallback.contains("generativelanguage.googleapis.com"),
            "{fallback}"
        );
    }

    #[tokio::test]
    async fn with_ai_off_the_fallback_is_off() {
        let mut config = config(None);
        config.fallback = Some(endpoint(AiProvider::Mock));
        let gateway = Gateway::new(&config, Bangkok, testing::unreachable());
        assert!(gateway.inner.fallback.is_none());
        assert_eq!(enabled(&gateway), []);
    }

    /// NFR-REL-06: the primary works on without a fallback that can't be built.
    #[tokio::test]
    async fn a_fallback_without_an_adapter_is_left_out() {
        let mut config = config(Some(endpoint(AiProvider::Mock)));
        config.fallback = Some(AiEndpoint {
            // Can't be sent in an HTTP header
            api_key: Some(Secret::new("two\nlines")),
            ..endpoint(AiProvider::Gemini)
        });
        let gateway = Gateway::new(&config, Bangkok, testing::unreachable());
        assert!(gateway.inner.fallback.is_none());
        assert_eq!(enabled(&gateway), Feature::ALL);
    }

    /// A call that can't be counted reached no provider, so it isn't the primary's failure.
    #[tokio::test]
    async fn a_call_that_cannot_be_counted_asks_no_provider() {
        let (primary, fallback) = (Mock::scripted([]), Mock::scripted([]));
        let gateway = with_fallback(&config(None), &primary, &fallback);
        assert_eq!(
            gateway.complete(ask(Feature::Assistant)).await,
            Err(Unavailable::Failed)
        );
        assert!(primary.requests().is_empty());
        assert!(fallback.requests().is_empty());
    }

    #[tokio::test]
    async fn features_turned_off_in_config_stay_off() {
        let mut config = config(None);
        config.features.listing = false;
        config.features.voice = false;
        let adapter = Adapter::Mock(Mock::new("mock"));
        let gateway = Gateway::for_test(
            &config,
            Some(adapter),
            testing::unreachable(),
            Duration::from_secs(1),
        );
        assert_eq!(
            enabled(&gateway),
            [
                Feature::Assistant,
                Feature::Moderation,
                Feature::SlipReading,
                Feature::VisualSearch
            ]
        );
        assert_eq!(
            gateway.complete(ask(Feature::Listing)).await,
            Err(Unavailable::Disabled)
        );
    }

    #[tokio::test]
    async fn with_ai_off_every_feature_is_off() {
        let off = Gateway::new(&config(None), Bangkok, testing::unreachable());
        assert_eq!(enabled(&off), []);
        assert_eq!(
            off.complete(ask(Feature::Assistant)).await,
            Err(Unavailable::Disabled)
        );

        // Claude takes no audio, so voice messages stay off (FR-AIP-04)
        let claude = config(Some(endpoint(AiProvider::Claude)));
        let claude = Gateway::new(&claude, Bangkok, testing::unreachable());
        let without_voice: Vec<Feature> = Feature::ALL
            .into_iter()
            .filter(|feature| *feature != Feature::Voice)
            .collect();
        assert_eq!(enabled(&claude), without_voice);

        let mock = Gateway::new(
            &config(Some(endpoint(AiProvider::Mock))),
            Bangkok,
            testing::unreachable(),
        );
        assert_eq!(enabled(&mock), Feature::ALL);
    }
}
