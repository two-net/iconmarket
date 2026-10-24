//! Shops & catalog (SRS §2.1): shops, products, images, categories and moderation.
//!
//! [`categories`] holds the categories in the order admins set, [`browse`] is "ซื้อของ", and
//! [`shops`] is "เปิดร้าน" and "ร้านของฉัน".

pub mod browse;
pub mod categories;
pub mod images;
pub mod shops;

use axum::Router;
use axum::routing::get;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/img/p/{image_id}", get(images::serve_product))
        .route("/img/s/{token}", get(images::serve_slip))
}
