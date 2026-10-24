//! Products (FR-PROD). Built by #25 and #26; #24 adds what a buyer can do with a product while
//! its shop is closed.
//!
//! A product's [`Availability`] is what its card says of it (FR-SRCH-07): how many are left, or
//! that it is made to order, sold out (FR-PROD-06) or of a closed shop (FR-SHOP-05). Only an
//! orderable one can go in a cart (#29) or be checked out (#32).

#![allow(
    dead_code,
    reason = "the product card (#28), the cart (#29) and checkout (#32) are built by later issues"
)]

use super::shops;
use crate::persistence::entity::{product, shop};

/// Whether a buyer can order a product now, as its card says (FR-SRCH-07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// *เหลือ 5*: this many left for new orders (FR-PROD-05)
    Left(i32),
    /// *สั่งได้*: unlimited or made to order
    Orderable,
    /// *หมด* (FR-PROD-06)
    SoldOut,
    /// *ร้านปิด*: its shop takes no orders now (FR-SHOP-05)
    ShopClosed,
}

impl Availability {
    /// That of `product` of `shop`. Whether the product is for sale at all, `ACTIVE`, is the
    /// caller's check: lists show only those (FR-SRCH-02), and the cart flags the others
    /// (FR-CART-03). A shop that doesn't take orders makes it [`ShopClosed`](Self::ShopClosed)
    /// whatever its stock; one not `APPROVED` has its products hidden anyway (FR-SHOP-10).
    pub fn of(product: &product::Model, shop: &shop::Model) -> Self {
        if !shops::takes_orders(shop) {
            return Self::ShopClosed;
        }
        match product.stock {
            None => Self::Orderable,
            Some(0) => Self::SoldOut,
            Some(left) => Self::Left(left),
        }
    }

    /// Whether it can be added to a cart or checked out.
    pub fn orderable(self) -> bool {
        matches!(self, Self::Left(_) | Self::Orderable)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::thai::Money;

    fn shop(status: shop::Status, is_open: bool) -> shop::Model {
        let at = Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap();
        shop::Model {
            id: 1,
            owner_id: 1,
            name: "ร้านป้าแดง".to_owned(),
            description: "ไข่ไก่".to_owned(),
            status,
            is_open,
            opening_hours: None,
            offers_pickup: true,
            offers_delivery: false,
            delivery_fee: Money::ZERO,
            minimum_order: None,
            promptpay_id: None,
            phone: None,
            show_phone: true,
            pickup_address: None,
            pickup_latitude: None,
            pickup_longitude: None,
            rejection_reason: None,
            created_at: at,
            updated_at: at,
            deleted_at: None,
        }
    }

    fn product(stock: Option<i32>) -> product::Model {
        let at = Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap();
        product::Model {
            id: 1,
            shop_id: 1,
            category_id: 1,
            name: "ไข่ไก่".to_owned(),
            description: None,
            price: Money::from_baht(110),
            unit: "แผง".to_owned(),
            stock,
            status: product::Status::Active,
            created_at: at,
            updated_at: at,
            deleted_at: None,
        }
    }

    /// FR-SRCH-07, FR-PROD-06: an open shop's product is orderable while it has stock.
    #[test]
    fn an_open_shops_product_goes_by_its_stock() {
        let open = shop(shop::Status::Approved, true);
        let cases = [
            (Some(5), Availability::Left(5), true),
            (None, Availability::Orderable, true),
            (Some(0), Availability::SoldOut, false),
        ];
        for (stock, availability, orderable) in cases {
            assert_eq!(Availability::of(&product(stock), &open), availability);
            assert_eq!(availability.orderable(), orderable, "{availability:?}");
        }
    }

    /// FR-SHOP-05, FR-SHOP-10: a closed shop's products can't be ordered, sold out or not, and
    /// nor can those of a shop that isn't approved.
    #[test]
    fn a_closed_shops_products_cannot_be_ordered() {
        for shop in [
            shop(shop::Status::Approved, false),
            shop(shop::Status::Suspended, true),
            shop(shop::Status::Pending, true),
            shop(shop::Status::Closed, true),
        ] {
            for stock in [Some(5), None, Some(0)] {
                let availability = Availability::of(&product(stock), &shop);
                assert_eq!(availability, Availability::ShopClosed, "{shop:?}");
                assert!(!availability.orderable());
            }
        }
    }
}
