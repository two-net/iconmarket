//! Cart & orders (SRS §2.1): cart, checkout, stock reservation and the order lifecycle.
//! Built by #29, #32, #33 and #34.

/// The action of "ใส่ตะกร้า" on a product's card while it can be ordered (FR-SRCH-07); its
/// `product` is the product's ID. #29 asks how many and puts them in the cart (FR-CART-01).
pub const ADD_TO_CART: &str = "cart.add";
pub const PRODUCT_ID: &str = "product";
