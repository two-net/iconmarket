//! Shops & catalog (SRS §2.1): shops, products, images, categories and moderation.
//! Images and their endpoints are built by #11.

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/img/p/{image_id}", get(product_image))
        .route("/img/s/{token}", get(slip_image))
}

/// Product and shop images, public by random ID (§3.4).
async fn product_image() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// Payment-slip images, behind an expiring single-purpose token (§3.4).
async fn slip_image() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
