//! "ประกาศ" (FR-ADM-07): an admin writes a text and may add a photo, sees the announcement as
//! its recipients will, with how many followers it goes to and what that costs of the push
//! quota, and confirms. It is then pushed to every user who follows the OA, but the admin, in one
//! request each (FR-NOT-02, FR-NOT-04): the only marketing push (NFR-COST-01). Sending it is
//! audited (FR-ADM-08).
//!
//! The text and photo are asked for in a [flow](AnnouncementFlow), whose preview saves the
//! announcement unsent and gives it the photo, so the draft sweep can't take that before the
//! confirm button expires. The button is a pending action ([`SendAnnouncement`]): nothing is
//! sent until it is tapped, and a second tap sends nothing more (FR-UI-13). Starting another
//! announcement deletes the admin's unsent ones, so an older preview's button sends nothing; one
//! never confirmed is deleted once its button expires ([`delete_unsent`]), and one sent goes
//! with the audit log ([`delete_sent`]).

#[cfg(test)]
mod tests;

use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::sea_query::Query;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseTransaction, DbErr,
    EntityTrait, QueryFilter, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;
use url::Url;

use super::audit::{self, Entry, Target};
use crate::catalog::images::{self, Owner, Rejected};
use crate::config::Config;
use crate::dispatcher::flow::{self, Answer, Flow, Session};
use crate::dispatcher::menu::{self, Item};
use crate::dispatcher::pending::{self, Applied, Proposal, Stale};
use crate::dispatcher::postback::Postback;
use crate::dispatcher::{Input, Turn};
use crate::line::{self, Action, Message};
use crate::notifications::templates::{Templates, labels, with_choices};
use crate::notifications::{self, Kind, To};
use crate::persistence::entity::announcement;
use crate::persistence::entity::image::{self, OwnerType};
use crate::users::access::{self, Denied, authorize};

/// What the actions of the buttons of "ประกาศ", which is [`Item::Announcements`], start with
pub const PREFIX: &str = "announcements.";
/// "แก้ไข" under a preview: writes the announcement anew
pub const EDIT: &str = "announcements.edit";

/// The longest text, leaving room for the heading within LINE's 5,000 characters
pub const TEXT_CHARS: usize = 4000;

/// The announcement being written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementFlow {
    /// Empty until typed
    text: String,
    /// Sent in this flow: a draft image of the admin's
    photo: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    Text,
    /// Optional
    Photo,
}

impl Flow for AnnouncementFlow {
    const NAME: &'static str = "announcement";
    type Step = Step;
}

/// Sending announcement `announcement_id`, once confirmed (FR-UI-13). Its photo's link is kept
/// too, as `app.baseUrl` isn't at hand when it is applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendAnnouncement {
    announcement_id: i64,
    image_url: Option<Url>,
}

impl Proposal for SendAnnouncement {
    const KIND: &'static str = "announcement.send";

    fn requires(&self) -> access::Action {
        access::Action::Administer
    }

    /// Queues the announcement for every follower but the admin, if it is still unsent: one the
    /// admin has since replaced is gone.
    async fn apply(
        &self,
        _id: Uuid,
        txn: &DatabaseTransaction,
        turn: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        // Locked, so that two taps at once can't both send it
        let Some(announcement) = announcement::Entity::find_by_id(self.announcement_id)
            .filter(announcement::Column::SentAt.is_null())
            .lock_exclusive()
            .one(txn)
            .await?
        else {
            return Ok(Applied::Changed);
        };
        let admin_id = turn.actor.user.id;
        // As many as are queued below, in the same transaction
        let recipients = notifications::followers(txn, admin_id).await?;
        if recipients == 0 {
            return Ok(Applied::Done(nobody(turn)));
        }
        announcement::ActiveModel {
            id: Unchanged(announcement.id),
            sent_at: Set(Some(turn.now)),
            recipients: Set(Some(i32::try_from(recipients).unwrap_or(i32::MAX))),
            ..Default::default()
        }
        .update(txn)
        .await?;
        let entry = Entry {
            actor_id: Some(admin_id),
            action: audit::Action::AnnouncementSent,
            target: Target::Announcement(announcement.id),
            reason: None,
            details: audit::details([
                ("recipients", json!(recipients)),
                ("photo", json!(self.image_url.is_some())),
            ]),
        };
        audit::record(txn, turn.now, [entry]).await?;
        let messages = messages(turn.templates, &announcement.text, self.image_url.as_ref());
        turn.notify(
            Kind::Announcement,
            To::Followers { except: admin_id },
            messages,
        );
        Ok(Applied::Done(vec![
            turn.templates.announcement_sending(recipients),
        ]))
    }

    /// When it was sent, if it was; otherwise that the button expired, with "ประกาศ" to write
    /// it anew. To an admin only.
    async fn current_state(stale: Stale<Self>, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
            return Ok(vec![turn.denied(denied)]);
        }
        let found = match stale.proposal {
            Some(proposal) => {
                announcement::Entity::find_by_id(proposal.announcement_id)
                    .one(turn.db)
                    .await?
            }
            None => None,
        };
        let templates = turn.templates;
        Ok(vec![match found {
            Some(announcement::Model {
                sent_at: Some(at),
                recipients,
                ..
            }) => {
                let recipients = recipients.and_then(|n| u64::try_from(n).ok());
                templates.announcement_sent_already(at, recipients.unwrap_or(0))
            }
            _ => {
                let again = Item::Announcements.postback();
                templates.announcement_gone(again.button(labels::ANNOUNCEMENTS.clone()))
            }
        }])
    }
}

/// A tap on "ประกาศ", or on "แก้ไข" under a preview: starts writing an announcement.
pub async fn tapped(turn: &Turn<'_>, postback: &Postback) -> Result<Vec<Message>, DbErr> {
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        return Ok(vec![turn.denied(denied)]);
    }
    let action = postback.action();
    if action != Item::Announcements.action() && action != EDIT {
        return Ok(vec![turn.templates.denied(Denied::NotFound)]);
    }
    // An older preview's button would send what this one replaces
    let unsent = Condition::all()
        .add(announcement::Column::AdminId.eq(turn.actor.user.id))
        .add(announcement::Column::SentAt.is_null());
    delete(turn.db, unsent).await?;
    let session = Session {
        step: Step::Text,
        draft: AnnouncementFlow {
            text: String::new(),
            photo: None,
        },
    };
    let ask = turn.templates.ask_announcement_text(TEXT_CHARS);
    flow::start(turn, session, vec![ask]).await
}

/// The answer to the step the admin is on (FR-UI-12 step 3): the text, then a photo or "ข้าม",
/// which leads to the preview. A photo sent for the text is answered here, so it isn't taken for
/// a product to list (FR-UI-14); `None` for other input, such as a location, so routing goes on.
pub async fn answer(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    session: Session<AnnouncementFlow>,
    input: &Input,
) -> Result<Option<Vec<Message>>, DbErr> {
    // The admin role can be lost mid-flow (FR-ADM-01)
    if let Err(denied) = authorize(turn.db, &turn.actor, access::Action::Administer).await? {
        let done = Answer::<AnnouncementFlow>::Done(vec![turn.denied(denied)]);
        return flow::answered(turn, done).await.map(Some);
    }
    let templates = turn.templates;
    let Session { step, mut draft } = session;
    let answer = match (step, input) {
        (Step::Text, Input::Text(text)) => {
            let text = text.trim();
            if text.is_empty() {
                Answer::Again(vec![templates.ask_announcement_text(TEXT_CHARS)])
            } else if text.chars().count() > TEXT_CHARS {
                Answer::Again(vec![templates.text_too_long(TEXT_CHARS)])
            } else {
                draft.text = text.to_owned();
                let session = Session {
                    step: Step::Photo,
                    draft,
                };
                Answer::Next(session, ask_photo(templates))
            }
        }
        (Step::Text, Input::Image { .. }) => {
            Answer::Again(vec![templates.announcement_text_expected()])
        }
        (Step::Photo, Input::Image { message_id }) => {
            match photo(turn, config, line, message_id).await? {
                Ok(id) => {
                    draft.photo = Some(id);
                    return preview(turn, config, line, draft).await.map(Some);
                }
                Err(rejected) => {
                    let why = Message::text(templates.image_rejected(rejected));
                    Answer::Again(with_choices(vec![why], photo_choices()))
                }
            }
        }
        (Step::Photo, Input::Text(text)) if text.trim() == labels::SKIP.as_str() => {
            return preview(turn, config, line, draft).await.map(Some);
        }
        (Step::Photo, Input::Text(_)) => {
            let why = templates.photo_expected(&labels::SKIP);
            Answer::Again(with_choices(vec![why], photo_choices()))
        }
        _ => return Ok(None),
    };
    flow::answered(turn, answer).await.map(Some)
}

/// Downloads the photo the admin sent in `message_id` (FR-LINE-09) and keeps it as their draft.
async fn photo(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    message_id: &str,
) -> Result<Result<Uuid, Rejected>, DbErr> {
    let processed = match images::receive(line, message_id, &config.image).await {
        Ok(Ok(processed)) => processed,
        Ok(Err(rejected)) => return Ok(Err(rejected)),
        Err(e) => {
            warn!(error = %e, "cannot download an announcement's photo");
            return Ok(Err(Rejected::Unavailable));
        }
    };
    let owner = Owner::Draft(turn.actor.user.id);
    images::store(turn.db, owner, processed, &config.image, turn.now).await
}

/// Ends the flow with the announcement `draft` as its recipients will see it, saved unsent with
/// its photo, and what sending it costs, with the button that sends it. With no one to send it
/// to, it is dropped.
async fn preview(
    turn: &Turn<'_>,
    config: &Config,
    line: &line::Client,
    draft: AnnouncementFlow,
) -> Result<Vec<Message>, DbErr> {
    let templates = turn.templates;
    let admin_id = turn.actor.user.id;
    let recipients = notifications::followers(turn.db, admin_id).await?;
    if recipients == 0 {
        if let Some(photo) = draft.photo {
            images::delete_draft(turn.db, photo, admin_id).await?;
        }
        return flow::answered(turn, Answer::<AnnouncementFlow>::Done(nobody(turn))).await;
    }

    let announcement = announcement::ActiveModel {
        admin_id: Set(admin_id),
        text: Set(draft.text.clone()),
        created_at: Set(turn.now),
        sent_at: Set(None),
        recipients: Set(None),
        ..Default::default()
    }
    .insert(turn.db)
    .await?;
    let image_url = match draft.photo {
        Some(photo) => {
            let owner = Owner::Announcement(announcement.id);
            if !images::adopt(turn.db, photo, admin_id, owner).await? {
                // Swept, the flow having been idle too long
                announcement::Entity::delete_by_id(announcement.id)
                    .exec(turn.db)
                    .await?;
                let mut ask = vec![templates.announcement_photo_lost()];
                ask.extend(ask_photo(templates));
                let session = Session {
                    step: Step::Photo,
                    draft: AnnouncementFlow {
                        photo: None,
                        ..draft
                    },
                };
                return flow::answered(turn, Answer::Next(session, ask)).await;
            }
            Some(images::product_image_url(&config.app.base_url, photo))
        }
        None => None,
    };

    // LINE's own count, as pushes to the village group cost one per member (FR-NOT-07)
    let quota = line
        .quota()
        .await
        .inspect_err(|e| warn!(error = %e, "cannot read the push quota; the log's count is shown"))
        .ok();
    let pushed = notifications::stats(turn.db, turn.now, templates.time_zone())
        .await?
        .sent;
    let send = SendAnnouncement {
        announcement_id: announcement.id,
        image_url: image_url.clone(),
    };
    let confirm = pending::propose(turn, &send).await?;
    let confirm = confirm.button(labels::SEND_ANNOUNCEMENT.clone());
    let edit = Postback::new(EDIT).button(labels::EDIT.clone());
    let mut reply = messages(templates, &draft.text, image_url.as_ref());
    reply.push(templates.confirm_announcement(recipients, quota, pushed, confirm, edit));
    flow::answered(turn, Answer::<AnnouncementFlow>::Done(reply)).await
}

/// The announcement as its recipients see it: `text`, then the photo at `image_url` if it has
/// one, in one push request.
fn messages(templates: &Templates, text: &str, image_url: Option<&Url>) -> Vec<Message> {
    let photo = image_url.map(|url| Message::image(url, url));
    std::iter::once(templates.announcement(text))
        .chain(photo)
        .collect()
}

/// What asks for the photo, with its choices.
fn ask_photo(templates: &Templates) -> Vec<Message> {
    with_choices(vec![templates.ask_announcement_photo()], photo_choices())
}

/// The Quick Replies of the photo step, besides the cancel button the flow adds (FR-UI-04).
fn photo_choices() -> Vec<Action> {
    vec![
        Action::Camera {
            label: labels::TAKE_PHOTO.clone(),
        },
        Action::CameraRoll {
            label: labels::CHOOSE_PHOTO.clone(),
        },
        Action::message(labels::SKIP.clone(), labels::SKIP.as_str()),
    ]
}

/// No one but the admin follows the OA, so nothing is sent.
fn nobody(turn: &Turn<'_>) -> Vec<Message> {
    vec![
        turn.templates.no_announcement_recipients(),
        menu::main_menu(&turn.actor),
    ]
}

/// Deletes, as of `before`, the announcements never confirmed: their confirm buttons expired, as
/// they last `session.pendingActionMinutes`. Returns how many were deleted.
pub async fn delete_unsent(db: &impl ConnectionTrait, before: DateTimeUtc) -> Result<u64, DbErr> {
    let unsent = Condition::all()
        .add(announcement::Column::SentAt.is_null())
        .add(announcement::Column::CreatedAt.lt(before));
    delete(db, unsent).await
}

/// Deletes the announcements sent before `before`, as their audit entries are (§5.4). Returns
/// how many were deleted.
pub async fn delete_sent(db: &impl ConnectionTrait, before: DateTimeUtc) -> Result<u64, DbErr> {
    delete(
        db,
        Condition::all().add(announcement::Column::SentAt.lt(before)),
    )
    .await
}

/// Deletes the announcements `which` picks, with their photos. Returns how many were deleted.
async fn delete(db: &impl ConnectionTrait, which: Condition) -> Result<u64, DbErr> {
    let ids = Query::select()
        .column(announcement::Column::Id)
        .from(announcement::Entity)
        .cond_where(which.clone())
        .to_owned();
    image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(OwnerType::Announcement))
        .filter(image::Column::OwnerId.in_subquery(ids))
        .exec(db)
        .await?;
    let deleted = announcement::Entity::delete_many()
        .filter(which)
        .exec(db)
        .await?;
    Ok(deleted.rows_affected)
}
