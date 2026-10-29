//! Cart & orders (SRS §2.1): cart, checkout, stock reservation and the order lifecycle.
//! Built by #29, #32, #33, #34 and #35; [`periods`] counts the orders placed of late, for
//! "สถิติ" and "ยอดขาย".

pub mod actions;
pub mod cart;
pub mod checkout;
mod contact;
mod expiry;
pub mod lifecycle;
pub mod lists;
pub mod periods;

pub use expiry::expire;

/// The action of "ใส่ตะกร้า" on a product's card while it can be ordered (FR-SRCH-07); its
/// `product` is the product's ID. The [cart] asks how many and puts them in it (FR-CART-01).
pub const ADD_TO_CART: &str = "cart.add";
pub const PRODUCT_ID: &str = "product";

/// The action of "สั่งซื้อเลย" once a product is in the cart, of "สั่งซื้อร้านนี้" on a shop's
/// card in it (FR-CART-01, FR-CART-03), and of "แก้ไข" on checkout's summary; its `shop` is the
/// shop's ID. [Checkout](checkout) orders the cart's lines of that shop (FR-CART-04).
pub const CHECKOUT: &str = "checkout";
pub const SHOP_ID: &str = "shop";

/// The actions of an order's buttons (Table 4.6-B) start with this, and carry the order's ID as
/// their `order`. [Acting on an order](actions) checks the user may still do it, and that the
/// order is where the button found it.
pub const PREFIX: &str = "order.";
pub const ORDER_ID: &str = "order";

/// The buyer's "ยกเลิกออเดอร์" on the card of an order the shop is yet to accept (FR-CART-08,
/// FR-ORD-06)
pub const CANCEL_ORDER: &str = "order.cancel";
/// The buyer's "ขอยกเลิก" once it is accepted (FR-ORD-07)
pub const REQUEST_CANCEL: &str = "order.request_cancel";
/// The buyer's "ได้รับของแล้ว" (FR-ORD-09)
pub const RECEIVED: &str = "order.received";
/// The seller's "รับออเดอร์" and "ปฏิเสธ" (FR-ORD-03), which asks why (FR-ORD-04)
pub const ACCEPT: &str = "order.accept";
pub const REJECT: &str = "order.reject";
/// The seller's "พร้อมให้รับ", "กำลังไปส่ง" and "ส่งมอบแล้ว"
pub const READY: &str = "order.ready";
pub const DELIVERING: &str = "order.delivering";
pub const COMPLETE: &str = "order.complete";
/// The seller's "ยกเลิกออเดอร์" once it is accepted, which asks why (FR-ORD-04)
pub const SELLER_CANCEL: &str = "order.seller_cancel";
/// The seller's answers to the buyer's "ขอยกเลิก": "ยกเลิกออเดอร์" and "ไม่ยกเลิก" (FR-ORD-07)
pub const GRANT_CANCEL: &str = "order.grant_cancel";
pub const KEEP: &str = "order.keep";
/// An admin's "ยกเลิกออเดอร์", which asks why (FR-ORD-12)
pub const ADMIN_CANCEL: &str = "order.admin_cancel";
/// The seller's, or an admin's, "ได้รับเงินแล้ว" and "คืนเงินแล้ว" (FR-PAY-07 – FR-PAY-09)
pub const MARK_PAID: &str = "order.paid";
pub const MARK_REFUNDED: &str = "order.refunded";
/// The seller's "ยังไม่ได้รับ" when asked whether an order handed over was paid (FR-PAY-07)
pub const NOT_PAID: &str = "order.not_paid";

/// "ดูรายละเอียด" on an order's card in "คำสั่งซื้อของฉัน" or "ออเดอร์ร้าน": the order in full,
/// for its buyer, its seller or an admin (FR-ORD-10). [Lists](lists) show it, not
/// [acting on it](actions).
pub const VIEW: &str = "order.view";
