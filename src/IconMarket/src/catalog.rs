//! Shops & catalog (SRS §2.1): shops, products, images, categories and moderation.
//!
//! [`categories`] holds the categories in the order admins set, [`browse`] is "ซื้อของ",
//! [`shops`] is "เปิดร้าน", "ร้านของฉัน", "เปิด/ปิดร้าน" and "ตั้งค่าร้าน", and [`products`]
//! is "เพิ่มสินค้า" and "สินค้าของฉัน" and says whether a product can be ordered.

pub mod browse;
pub mod categories;
pub mod images;
pub mod products;
pub mod shops;

use axum::Router;
use axum::routing::get;

use crate::http::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/img/p/{image_id}", get(images::serve_product))
        .route("/img/s/{token}", get(images::serve_slip))
}
