//! Webhook receiver (SRS §2.1, §3.2): verifies LINE signatures (FR-LINE-02), stores events in
//! the inbox and only then acknowledges LINE (FR-LINE-03, NFR-REL-01). A redelivered event is
//! stored once (FR-LINE-04). The dispatcher processes the inbox, woken once events are stored.

mod inbox;
mod signature;
#[cfg(test)]
mod tests;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tracing::{error, warn};

use crate::http::AppState;

pub use signature::Verifier;

const SIGNATURE: &str = "x-line-signature";

pub fn router() -> Router<AppState> {
    Router::new().route("/webhook/line", post(receive))
}

/// Answers 200 only once every event in the request is stored, and 5xx if they can't be, so
/// that LINE redelivers. A request LINE didn't sign gets 400 and touches nothing.
async fn receive(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    if !state.webhook.verify(headers.get(SIGNATURE), &body) {
        warn!("webhook request refused: missing or invalid signature");
        return StatusCode::BAD_REQUEST;
    }
    let events = match inbox::parse(&body) {
        Ok(events) => events,
        Err(e) => {
            warn!(error = %e, "webhook request refused");
            return StatusCode::BAD_REQUEST;
        }
    };
    // The console's webhook verification sends no events
    if events.is_empty() {
        return StatusCode::OK;
    }
    match inbox::store(&state.db, events).await {
        Ok(()) => {
            state.wakeup.notify();
            StatusCode::OK
        }
        Err(e) => {
            error!(error = %e, "cannot store webhook events; LINE will redeliver them");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}
