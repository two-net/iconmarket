//! Contact data on orders (FR-ORD-11, NFR-PRIV-04): what each party of an order sees of how to
//! reach the other, by where the order is now. Every card and notice of an order takes it from
//! [`shown`], so no view shows more.
//!
//! - The seller of an order yet to be accepted sees its buyer's display name and zone, enough to
//!   decide; the buyer sees nothing of the shop yet.
//! - Once it is accepted, while it is active, the seller also sees the buyer's phone if they
//!   gave one, the full address and location it is delivered to, and the note; the buyer sees
//!   where to pick it up, for pickup, and the shop's phone unless its seller hides it
//!   (FR-SHOP-08).
//! - Once it is over, neither sees any: the seller keeps only the buyer's display name, to know
//!   whom an order was for, or who owes it (FR-PAY-07).

use crate::notifications::templates::{self, Contact, OrderContact};
use crate::persistence::entity::order::{self, Fulfillment, Status};
use crate::persistence::entity::order_event::ActorType;
use crate::persistence::entity::{shop, user};

/// What `viewer` sees of how to reach the other party of `order`, at `shop` by `buyer`, as it is
/// now. An admin sees none of it.
pub(super) fn shown<'a>(
    order: &'a order::Model,
    shop: &'a shop::Model,
    buyer: &'a user::Model,
    viewer: ActorType,
) -> OrderContact<'a> {
    let name = buyer.display_name.as_deref();
    match viewer {
        ActorType::Seller if order.status == Status::Pending => OrderContact::Buyer {
            name,
            // The zone of the address it is delivered to, or the buyer's own for pickup
            zone: order.delivery_zone.as_deref().or(buyer.zone.as_deref()),
        },
        ActorType::Seller if order.status.is_active() => OrderContact::BuyerFull {
            contact: Contact {
                buyer: name,
                address: delivery_address(order),
                phone: buyer.phone.as_deref(),
                note: order.note.as_deref(),
            },
            location: order.delivery_latitude.zip(order.delivery_longitude),
        },
        ActorType::Seller => OrderContact::Buyer { name, zone: None },
        ActorType::Buyer if order.status.is_active() => {
            let pickup = order.fulfillment == Fulfillment::Pickup;
            OrderContact::Shop {
                pickup: shop.pickup_address.as_deref().filter(|_| pickup),
                location: (shop.pickup_latitude.zip(shop.pickup_longitude)).filter(|_| pickup),
                phone: shop.phone.as_deref().filter(|_| shop.show_phone),
            }
        }
        ActorType::Buyer | ActorType::Admin | ActorType::System => OrderContact::None,
    }
}

/// Where `order` is delivered, from the copy of the address it keeps; `None` for pickup.
pub(super) fn delivery_address(order: &order::Model) -> Option<String> {
    match (&order.delivery_zone, &order.delivery_house_number) {
        (Some(zone), Some(house_number)) => Some(templates::address_line(
            zone,
            house_number,
            order.delivery_landmark_note.as_deref(),
        )),
        _ => None,
    }
}
