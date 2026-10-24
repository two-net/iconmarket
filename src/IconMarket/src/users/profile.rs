//! "ข้อมูลของฉัน" (FR-ONB-05, NFR-PRIV-03): the user sees all the personal data held about
//! them, and corrects their address, phone and location through the [address](super::address)
//! flow. Personal data is what the entity models mark as such (DR-10): the user's own row and
//! their shop's are shown as they are; the orders, payment slips and logs that hold more of it
//! are listed with how long they are kept (§5.4).
//!
//! Like every function but help, it needs onboarding complete (FR-UI-12), and is open to
//! suspended users too (NFR-PRIV-03). Help adds it to its topics (FR-HELP-01, #22).

#[cfg(test)]
mod tests;

use sea_orm::{
    ColumnTrait, Condition, DbErr, EntityTrait, JoinType, PaginatorTrait, QueryFilter, QuerySelect,
    RelationTrait,
};

use super::access::{self, Denied, authorize};
use super::address::{self, Ask, Then};
use crate::config::Config;
use crate::dispatcher::Turn;
use crate::dispatcher::postback::Postback;
use crate::line::Message;
use crate::notifications::templates::{MyData, MyShop, Retention, labels};
use crate::persistence::entity::{order, payment_slip, user};
use crate::thai::Label;

/// The action that shows "ข้อมูลของฉัน"
pub const SHOW: &str = "my_data";
/// The action of its buttons that correct the address, phone or location; its `part` says
/// which
pub const EDIT: &str = "my_data.edit";
const PART: &str = "part";

/// The buttons that correct the address, phone and location: their `part`, what they ask for
/// and their label.
static PARTS: [(&str, Ask, &Label); 3] = [
    ("address", Ask::Address, &labels::EDIT_ADDRESS),
    ("phone", Ask::Phone, &labels::EDIT_PHONE),
    ("location", Ask::Location, &labels::EDIT_LOCATION),
];

/// A tap on "ข้อมูลของฉัน", or on one of its buttons.
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::MyData).await? {
        return Ok(vec![turn.templates.denied(denied)]);
    }
    let part = postback
        .name(PART)
        .and_then(|part| PARTS.iter().find(|(name, ..)| *name == part))
        .map(|&(_, ask, _)| ask);
    match (postback.action(), part) {
        (SHOW, _) => show(turn, config, &turn.actor.user).await,
        (EDIT, Some(ask)) => address::start(turn, config, ask, Then::MyData).await,
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// "ข้อมูลของฉัน" for `user`, as they are now, and their shop if they have one.
pub async fn show(
    turn: &Turn<'_>,
    config: &Config,
    user: &user::Model,
) -> Result<Vec<Message>, DbErr> {
    let orders_with_address = order::Entity::find()
        .filter(order::Column::BuyerId.eq(user.id))
        .filter(
            Condition::any()
                .add(order::Column::DeliveryZone.is_not_null())
                .add(order::Column::DeliveryHouseNumber.is_not_null())
                .add(order::Column::DeliveryLandmarkNote.is_not_null())
                .add(order::Column::DeliveryLatitude.is_not_null())
                .add(order::Column::DeliveryLongitude.is_not_null()),
        )
        .count(turn.db)
        .await?;
    // Slips are sent by buyers, so only theirs hold the user's data
    let slips = payment_slip::Entity::find()
        .join(JoinType::InnerJoin, payment_slip::Relation::Order.def())
        .filter(order::Column::BuyerId.eq(user.id))
        .filter(
            Condition::any()
                .add(payment_slip::Column::ImageId.is_not_null())
                .add(payment_slip::Column::AiReference.is_not_null()),
        )
        .count(turn.db)
        .await?;

    let point = |latitude: Option<f64>, longitude: Option<f64>| latitude.zip(longitude);
    let shop = turn.actor.shop.as_ref().map(|shop| MyShop {
        phone: shop.phone.as_deref(),
        promptpay_id: shop.promptpay_id.as_deref(),
        pickup_address: shop.pickup_address.as_deref(),
        pickup_location: point(shop.pickup_latitude, shop.pickup_longitude),
    });
    let retention = &config.retention;
    let ai_on = config.ai.primary.is_some();
    let edit = PARTS
        .iter()
        .map(|&(part, _, label)| {
            let postback = Postback::new(EDIT).with_name(PART, part);
            postback.button(label.clone())
        })
        .collect();
    let data = MyData {
        display_name: user.display_name.as_deref(),
        has_picture: user.picture_url.is_some(),
        line_user_id: user.line_user_id.as_deref(),
        consent: user.consent_version.as_deref().zip(user.consented_at),
        zone: user.zone.as_deref(),
        house_number: user.house_number.as_deref(),
        landmark_note: user.landmark_note.as_deref(),
        phone: user.phone.as_deref(),
        location: point(user.latitude, user.longitude),
        shop,
        orders_with_address,
        slips,
        retention: Retention {
            slip_days: retention.slip_days,
            webhook_event_days: retention.webhook_event_days,
            push_log_days: retention.push_log_days,
            ai_message_days: ai_on.then_some(retention.ai_message_days),
        },
        edit,
    };
    Ok(vec![turn.templates.my_data(&data)])
}
