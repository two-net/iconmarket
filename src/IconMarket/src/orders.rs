//! Cart & orders (SRS §2.1): cart, checkout, stock reservation and the order lifecycle.
//! Built by #29, #32, #33 and #34.

pub mod cart;
pub mod checkout;

/// The action of "ใส่ตะกร้า" on a product's card while it can be ordered (FR-SRCH-07); its
/// `product` is the product's ID. The [cart] asks how many and puts them in it (FR-CART-01).
pub const ADD_TO_CART: &str = "cart.add";
pub const PRODUCT_ID: &str = "product";

/// The action of "สั่งซื้อเลย" once a product is in the cart, of "สั่งซื้อร้านนี้" on a shop's
/// card in it (FR-CART-01, FR-CART-03), and of "แก้ไข" on checkout's summary; its `shop` is the
/// shop's ID. [Checkout](checkout) orders the cart's lines of that shop (FR-CART-04).
pub const CHECKOUT: &str = "checkout";
pub const SHOP_ID: &str = "shop";

/// The action of "ยกเลิกออเดอร์" on the card of an order the shop is yet to accept (FR-CART-08,
/// FR-ORD-06); its `order` is the order's ID. Cancelling is built by #33.
pub const CANCEL_ORDER: &str = "order.cancel";
pub const ORDER_ID: &str = "order";
