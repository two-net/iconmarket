//! "เปิด/ปิดร้าน" and "ตั้งค่าร้าน" (FR-SHOP-05 – FR-SHOP-09): the owner of an approved shop
//! opens or closes it, changes its details, and sees its card as buyers do.
//!
//! "เปิด/ปิดร้าน" switches the shop at a tap (FR-SHOP-05). The reply's button switches it back:
//! it names the state it sets, so tapping it again changes nothing (FR-UI-13). Closing changes
//! only whether the shop [takes orders](super::takes_orders): its products stay visible, marked
//! "ร้านปิด", and its orders go on.
//!
//! "ตั้งค่าร้าน" shows the shop's card and a button for each part (FR-SHOP-06). Each starts a
//! [flow](SettingsFlow) that asks for the part as the application does, showing what it is now,
//! and saves it as soon as it is answered; a change to delivering asks for the fee too, and
//! saves both. Cancelling, or letting the flow expire, changes nothing. The opening hours
//! (FR-SHOP-07) are a part only here. Whether buyers with an active order see the shop's phone
//! (FR-SHOP-08) is set at a tap, like opening. What the user types is never logged
//! (NFR-PRIV-05).
//!
//! A changed name, description or photo is [moderated](moderation) before it is saved (FR-SHOP-06):
//! found suspicious, the shop is saved `HELD` for review. Its owner goes on running it as before,
//! but buyers don't see it until an admin approves it again (#31).

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, TransactionTrait};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{
    Photo, ShopFlow, Step, Taken, after, card, details, front, keep_photo, offered, photo_lost,
    photo_url, prompt, refused_answer, subject, take_input,
};
use crate::ai;
use crate::catalog::images::{self, Owner};
use crate::catalog::moderation::{self, Flag, Held};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::Item;
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{self, Action, Message};
use crate::notifications::templates::labels;
use crate::orders::rating;
use crate::persistence::entity::shop;
use crate::users::access::{self, Denied, authorize};

/// What the actions of the buttons of "เปิด/ปิดร้าน" and "ตั้งค่าร้าน" start with
pub const PREFIX: &str = "shop.";
/// Opens the shop if its `to` is 1, or closes it if 0
pub const OPEN: &str = "shop.open";
/// Changes the part named in `p`
pub const PART: &str = "shop.part";
/// Shows the shop's phone to buyers with an active order if its `show` is 1, or hides it if 0
pub const PHONE: &str = "shop.phone";
/// The shop's card as buyers see it
pub const FRONT: &str = "shop.front";
const TO: &str = "to";
const WHICH: &str = "p";
const SHOW: &str = "show";

/// The parts "ตั้งค่าร้าน" changes, in the order it offers them
const PARTS: [Step; 11] = [
    Step::Name,
    Step::Description,
    Step::Photo,
    Step::OpeningHours,
    Step::PickupAddress,
    Step::PickupLocation,
    Step::Phone,
    Step::Fulfillment,
    Step::DeliveryFee,
    Step::MinimumOrder,
    Step::PromptPay,
];

/// "ตั้งค่าร้าน" changing a part of the shop: its details as they were when the part was
/// tapped, with the answers taken into them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsFlow(ShopFlow);

impl Flow for SettingsFlow {
    const NAME: &'static str = "shop_settings";
    type Step = Step;
}

/// The part of "ตั้งค่าร้าน" named `key` in its button.
fn part(key: &str) -> Option<Step> {
    PARTS.into_iter().find(|part| part.key() == key)
}

/// The button that closes the shop if it is `open`, or opens it.
pub fn switch(open: bool) -> Action {
    let (label, to) = if open {
        (&labels::CLOSE_SHOP, 0)
    } else {
        (&labels::REOPEN_SHOP, 1)
    };
    Postback::new(OPEN).with_int(TO, to).button(label.clone())
}

/// The button that shows the owner's shop as buyers see it (FR-SHOP-09).
pub fn front_button() -> Action {
    Postback::new(FRONT).button(labels::SHOP_FRONT.clone())
}

/// A tap on "เปิด/ปิดร้าน" or "ตั้งค่าร้าน", or on one of their buttons: only for the owner of
/// an approved shop (FR-SHOP-10).
pub async fn tapped(
    turn: &Turn<'_>,
    config: &Config,
    postback: &Postback,
) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let Some(shop) = &turn.actor.shop else {
        return Ok(vec![turn.templates.denied(Denied::NotAllowed)]);
    };
    let action = postback.action();
    if action == Item::OpenClose.action() {
        return set_open(turn, shop, !shop.is_open).await;
    }
    if action == Item::ShopSettings.action() {
        return show(turn, config, shop, None).await;
    }
    match (action, postback.int(TO), postback.int(SHOW)) {
        (OPEN, Some(to @ (0 | 1)), _) => set_open(turn, shop, to == 1).await,
        (PHONE, _, Some(show @ (0 | 1))) => set_phone_shown(turn, config, shop, show == 1).await,
        (FRONT, ..) => {
            let photo = photo_url(config, images::first(turn.db, Owner::Shop(shop.id)).await?);
            let rating = rating::of_shops(turn.db, &[shop.id]).await?;
            let front = front(shop, photo.as_ref(), rating.get(&shop.id).copied());
            let settings = Item::ShopSettings
                .postback()
                .button(labels::SHOP_SETTINGS.clone());
            Ok(turn.templates.shop_front_preview(&front, settings))
        }
        (PART, ..) => {
            let photo = images::first(turn.db, Owner::Shop(shop.id)).await?;
            let draft = ShopFlow::from_shop(shop, photo);
            match postback.name(WHICH).and_then(part) {
                Some(part) if offered(config, &draft, part) => {
                    let ask = prompt(turn, config, &turn.actor.user, &draft, part);
                    let session = Session {
                        step: part,
                        draft: SettingsFlow(draft),
                    };
                    flow::start(turn, session, ask).await
                }
                // One no longer offered, such as the fee of a shop that stopped delivering
                _ => show(turn, config, shop, None).await,
            }
        }
        _ => Ok(vec![turn.templates.denied(Denied::NotFound)]),
    }
}

/// Opens `shop` or closes it for now (FR-SHOP-05), unless it already is. Nothing else changes:
/// its products stay as they are, and its orders go on.
async fn set_open(turn: &Turn<'_>, shop: &shop::Model, open: bool) -> Result<Vec<Message>, DbErr> {
    let changed = shop::Entity::update_many()
        .set(shop::ActiveModel {
            is_open: Set(open),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(shop::Column::Id.eq(shop.id))
        .filter(shop::Column::IsOpen.ne(open))
        .exec(turn.db)
        .await?
        .rows_affected
        > 0;
    if changed {
        info!(shop_id = shop.id, open, "shop opened or closed");
    }
    let switch = switch(open);
    let reply = turn
        .templates
        .shop_open_state(&shop.name, open, changed, switch);
    Ok(vec![reply])
}

/// Shows `shop`'s phone to buyers with an active order, or hides it from them (FR-SHOP-08),
/// unless it already is; then "ตั้งค่าร้าน" again.
async fn set_phone_shown(
    turn: &Turn<'_>,
    config: &Config,
    shop: &shop::Model,
    show_phone: bool,
) -> Result<Vec<Message>, DbErr> {
    let changed = shop::Entity::update_many()
        .set(shop::ActiveModel {
            show_phone: Set(show_phone),
            updated_at: Set(turn.now),
            ..Default::default()
        })
        .filter(shop::Column::Id.eq(shop.id))
        .filter(shop::Column::ShowPhone.ne(show_phone))
        .exec(turn.db)
        .await?
        .rows_affected
        > 0;
    if changed {
        info!(shop_id = shop.id, show_phone, "shop phone shown or hidden");
    }
    let shop = shop::Model {
        show_phone,
        ..shop.clone()
    };
    let done = turn.templates.phone_visibility(show_phone);
    show(turn, config, &shop, Some(done)).await
}

/// "ตั้งค่าร้าน" for `shop`: its card, after `first` if given, and a button for each part that
/// applies, for the phone's visibility and for the card buyers see. That is at most 13, as many
/// Quick Replies as LINE takes.
async fn show(
    turn: &Turn<'_>,
    config: &Config,
    shop: &shop::Model,
    first: Option<Message>,
) -> Result<Vec<Message>, DbErr> {
    let photo_id = images::first(turn.db, Owner::Shop(shop.id)).await?;
    let photo = photo_url(config, photo_id);
    let draft = ShopFlow::from_shop(shop, photo_id);
    let mut buttons: Vec<Action> = PARTS
        .into_iter()
        .filter(|&part| offered(config, &draft, part))
        .map(|part| {
            let postback = Postback::new(PART).with_name(WHICH, part.key());
            postback.button(part.label().clone())
        })
        .collect();
    let (label, show) = if shop.show_phone {
        (&labels::HIDE_PHONE, 0)
    } else {
        (&labels::SHOW_PHONE, 1)
    };
    buttons.push(
        Postback::new(PHONE)
            .with_int(SHOW, show)
            .button(label.clone()),
    );
    buttons.push(front_button());

    let card = card(shop, photo.as_ref(), None, Vec::new());
    let mut messages: Vec<Message> = first.into_iter().collect();
    messages.push(turn.templates.shop(&card));
    messages.push(turn.templates.shop_settings(buttons));
    Ok(messages)
}

/// The answer to the part being changed (FR-UI-12 step 3). `None` when the step doesn't take
/// this kind of input, so routing goes on (FR-UI-14).
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    line: &line::Client,
    session: Session<SettingsFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    // A suspension mid-flow ends it (NFR-SEC-10)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::ManageShop).await? {
        let done = Answer::<SettingsFlow>::Done(vec![turn.denied(denied)]);
        return flow::answered(turn, done).await.map(Some);
    }
    let Session {
        step,
        draft: SettingsFlow(mut draft),
    } = session;
    let Some(taken) = take_input(turn, config, line, &mut draft, step, input).await? else {
        return Ok(None);
    };
    let user = &turn.actor.user;
    let messages = match taken {
        Ok(taken) => {
            let next = match taken {
                Taken::Kept => draft.missing().unwrap_or(Step::Summary),
                Taken::Changed => after(config, &draft, step),
            };
            // The summary of an application is, here, saving
            if next == Step::Summary {
                save(turn, config, ai, draft, step).await?
            } else {
                let ask = prompt(turn, config, user, &draft, next);
                let session = Session {
                    step: next,
                    draft: SettingsFlow(draft),
                };
                flow::answered(turn, Answer::Next(session, ask)).await?
            }
        }
        Err(refused) => {
            let again = refused_answer(turn, config, ai, &draft, step, input, refused).await;
            flow::answered(turn, Answer::<SettingsFlow>::Again(again)).await?
        }
    };
    Ok(Some(messages))
}

/// Saves the shop's details as `draft` has them, `step` being the last answered, and ends the
/// flow with "ตั้งค่าร้าน" again. The shop must still be approved, or held for review, and a new
/// photo still there; a draft left as the shop is, by "ใช้ของเดิม", writes nothing. Moderation may
/// hold it for review (FR-SHOP-06).
async fn save(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    mut draft: ShopFlow,
    step: Step,
) -> Result<Vec<Message>, DbErr> {
    let user_id = turn.actor.user.id;
    // Before anything is locked, as the AI may take a while
    let flag = moderate(turn, config, ai, &draft).await?;
    // The shop and its photo change together, or not at all
    let txn = turn.db.begin().await?;
    let shop = shop::Entity::find()
        .filter(shop::Column::OwnerId.eq(user_id))
        .lock_exclusive()
        .one(&txn)
        .await?;
    let shop = match shop {
        Some(shop) if matches!(shop.status, shop::Status::Approved | shop::Status::Held) => shop,
        shop => {
            txn.rollback().await?;
            let denied = match shop.map(|shop| shop.status) {
                Some(shop::Status::Suspended) => Denied::ShopSuspended,
                _ => Denied::NotAllowed,
            };
            let done = vec![turn.denied(denied)];
            return flow::answered(turn, Answer::<SettingsFlow>::Done(done)).await;
        }
    };
    let current = ShopFlow {
        revision: draft.revision,
        ..ShopFlow::from_shop(&shop, images::first(&txn, Owner::Shop(shop.id)).await?)
    };
    if current == draft {
        txn.rollback().await?;
        let unchanged = turn.templates.shop_settings_unchanged();
        let messages = show(turn, config, &shop, Some(unchanged)).await?;
        return flow::answered(turn, Answer::<SettingsFlow>::Done(messages)).await;
    }
    let mut update = shop::ActiveModel {
        updated_at: Set(turn.now),
        ..details(&draft)
    };
    // Unless it was held meanwhile
    let flag = flag.filter(|_| shop.status == shop::Status::Approved);
    if flag.is_some() {
        update.status = Set(shop::Status::Held);
    }
    let saved = shop::Entity::update_many()
        .set(update)
        .filter(shop::Column::Id.eq(shop.id))
        .exec_with_returning(&txn)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| DbErr::RecordNotFound(format!("shop {}", shop.id)))?;
    if !keep_photo(&txn, user_id, shop.id, draft.photo).await? {
        // The shop stays as it was
        txn.rollback().await?;
        let ask = photo_lost(turn, config, &mut draft);
        let session = Session {
            step: Step::Photo,
            draft: SettingsFlow(draft),
        };
        return flow::answered(turn, Answer::Next(session, ask)).await;
    }
    txn.commit().await?;

    info!(
        shop_id = shop.id,
        part = step.key(),
        held = flag.is_some(),
        "shop settings saved"
    );
    let saved_message = match &flag {
        Some(flag) => {
            moderation::hold(turn, Held::Shop(&saved), flag).await?;
            turn.templates.shop_held_saved()
        }
        None => turn.templates.shop_settings_saved(),
    };
    let messages = show(turn, config, &saved, Some(saved_message)).await?;
    flow::answered(turn, Answer::<SettingsFlow>::Done(messages)).await
}

/// Moderates the user's shop as `draft` has it (FR-SHOP-06), if that changes its name,
/// description or photo: why to hold it, if it should be. One held already isn't checked again.
async fn moderate(
    turn: &Turn<'_>,
    config: &Config,
    ai: &ai::Gateway,
    draft: &ShopFlow,
) -> Result<Option<Flag>, DbErr> {
    let Some(shop) = turn
        .actor
        .shop
        .as_ref()
        .filter(|shop| shop.status == shop::Status::Approved)
    else {
        return Ok(None);
    };
    let changes = draft.name.as_deref() != Some(shop.name.as_str())
        || draft.description.as_deref() != Some(shop.description.as_str())
        || matches!(draft.photo, Some(Photo::New(_)));
    if !changes {
        return Ok(None);
    }
    moderation::check(turn.db, config, ai, &subject(draft)).await
}
