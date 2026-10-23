//! Structured logs (NFR-MNT-05): one JSON object per line with time, level, module
//! (`target`) and the fields of the current span.
//!
//! Every webhook event is processed inside [`event_span`], so its lines also carry the
//! event's correlation ID and the internal user ID.
//!
//! Log internal IDs only: never names, phone numbers, addresses or message content
//! (NFR-PRIV-05), and never configuration secrets (CFG-06).

use std::fs::OpenOptions;

use anyhow::{Context, anyhow};
use tracing::Span;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LoggingConfig;

/// Installs the global logger. Keep the guard until exit: dropping it flushes buffered lines.
pub fn init(config: &LoggingConfig) -> anyhow::Result<WorkerGuard> {
    let (writer, guard) = if config.file.as_os_str().is_empty() {
        tracing_appender::non_blocking(std::io::stdout())
    } else {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.file)
            .with_context(|| format!("cannot open log file {}", config.file.display()))?;
        tracing_appender::non_blocking(file)
    };
    subscriber(config.level.into(), writer)
        .try_init()
        .map_err(|e| anyhow!("cannot install the logger: {e}"))?;
    Ok(guard)
}

fn subscriber<W>(level: LevelFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_max_level(level)
        .with_writer(writer)
        .finish()
}

/// The span a webhook event is processed in. Record the internal user ID, never the LINE
/// user ID, once it is known: `span.record("user_id", id)`.
#[allow(
    dead_code,
    reason = "used by the webhook receiver and dispatcher (#8, #14)"
)]
pub fn event_span(correlation_id: &str) -> Span {
    tracing::info_span!("event", correlation_id, user_id = tracing::field::Empty)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn lines(capture: &Capture) -> Vec<serde_json::Value> {
        let bytes = capture.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn lines_carry_time_level_module_and_event_ids() {
        let capture = Capture::default();
        let writer = capture.clone();
        tracing::subscriber::with_default(
            subscriber(LevelFilter::INFO, move || writer.clone()),
            || {
                let span = event_span("evt-01");
                span.record("user_id", 42);
                span.in_scope(|| tracing::info!("handled"));
            },
        );

        let lines = lines(&capture);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line["timestamp"].is_string(), "{line}");
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["target"], "icon_market::logging::tests");
        assert_eq!(line["message"], "handled");
        assert_eq!(line["span"]["correlation_id"], "evt-01");
        assert_eq!(line["span"]["user_id"], 42);
    }

    #[test]
    fn level_filters_lower_levels() {
        let capture = Capture::default();
        let writer = capture.clone();
        tracing::subscriber::with_default(
            subscriber(LevelFilter::WARN, move || writer.clone()),
            || {
                tracing::info!("dropped");
                tracing::warn!("kept");
            },
        );

        let lines = lines(&capture);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["message"], "kept");
    }
}
