//! Shops & catalog (SRS §2.1): shops, products, images, categories and moderation.
//!
//! [`categories`] holds the categories in the order admins set, and [`browse`] is "ซื้อของ".

pub mod browse;
pub mod categories;
pub mod images;

use axum::Router;
use axum::routing::get;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/img/p/{image_id}", get(images::serve_product))
        .route("/img/s/{token}", get(images::serve_slip))
}
