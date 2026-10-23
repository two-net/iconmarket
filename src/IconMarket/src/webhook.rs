//! Webhook receiver (SRS §2.1): verifies LINE signatures, stores events in the inbox and
//! acknowledges LINE. Built by #8.

use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/webhook/line", post(receive))
}

async fn receive() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
