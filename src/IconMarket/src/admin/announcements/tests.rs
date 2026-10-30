//! "ประกาศ" (FR-ADM-07, FR-ADM-08, NFR-COST-01): who may announce, what the flow asks and
//! refuses, the preview with what sending costs, that nothing is sent before the confirm, that
//! only followers get it, once, and that it is audited; an older preview, a cancelled one and
//! one never confirmed send nothing. Each event is handled in a transaction committed once it is
//! handled, with what it notifies queued in it. The `async` tests each start a throwaway
//! PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::sea_query::Expr;
use sea_orm::{DatabaseConnection, QueryOrder, TransactionTrait};
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::dispatcher::Location;
use crate::line::testing::{self as fake, Fake};
use crate::line::{Body, Quota, limits};
use crate::notifications::templates;
use crate::persistence::entity::{audit_log, push_log, session, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const PHOTO: &[u8] = include_bytes!("../../ai/provider/fixtures/media/slip.jpg");

const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_ADMIN: &str = "Ua2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";
const BUYER: &str = "Ub1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
const SUSPENDED: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const BLOCKED: &str = "Ub3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3";

const TEXT: &str = "พรุ่งนี้ตลาดนัดหน้าวัด 7 โมงเช้า\nร้านไหนจะมาขาย แจ้งในกลุ่มได้เลย";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    line: Fake,
    config: Config,
    templates: Templates,
    /// Writes the announcements
    admin: i64,
    /// Follows the OA, as do [`OTHER_ADMIN`] and a suspended user; one user blocked the OA, and
    /// one deleted their account
    buyer: i64,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let admin = |line_user_id| user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(line_user_id)
    };
    let admin_id = admin(ADMIN).insert(&db).await.unwrap().id;
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    let suspended = user::ActiveModel {
        status: Set(user::Status::Suspended),
        suspension_reason: Set(Some("ขายของปลอม".to_owned())),
        ..testing::new_user(SUSPENDED)
    };
    suspended.insert(&db).await.unwrap();
    let blocked = user::ActiveModel {
        is_following: Set(false),
        ..testing::new_user(BLOCKED)
    };
    blocked.insert(&db).await.unwrap();
    // Deleting an account takes its LINE user ID away (FR-ONB-07)
    let deleted = user::ActiveModel {
        line_user_id: Set(None),
        status: Set(user::Status::Deleted),
        ..testing::new_user("unused")
    };
    deleted.insert(&db).await.unwrap();
    admin(OTHER_ADMIN).insert(&db).await.unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        line: Fake::start().await,
        templates: Templates::new(&config.app),
        config,
        admin: admin_id,
        buyer,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `at` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh and what it
    /// notifies queued.
    async fn handle_at(
        &self,
        user_id: i64,
        at: DateTimeUtc,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, at)
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages, outbox, ..
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.config.notification, at)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        // Empty when the flow doesn't take the input, and routing goes on
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        messages
    }

    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        self.handle_at(user_id, t0(), handle).await
    }

    /// User `user_id` taps a button sending `data`, as routing hands it over.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        self.taps_at(user_id, data, t0()).await
    }

    async fn taps_at(&self, user_id: i64, data: &str, at: DateTimeUtc) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle_at(user_id, at, async |turn| {
            if postback.action() == pending::CONFIRM {
                pending::tapped(turn, &postback).await
            } else {
                tapped(turn, &postback).await
            }
        })
        .await
    }

    /// User `user_id` answers their announcement flow with `input`; `None` if the step doesn't
    /// take it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let line = self.line.client();
        let mut answered = None;
        self.handle(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<AnnouncementFlow>());
            let session = session.expect("writing an announcement");
            answered = answer(turn, &self.config, &line, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    async fn types(&self, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(self.admin, input).await.expect("an answer")
    }

    /// The admin sends a photo, which LINE serves as `bytes`.
    async fn sends_photo(&self, message_id: &str, bytes: &[u8]) -> Vec<Message> {
        let path = format!("/v2/bot/message/{message_id}/content");
        self.line
            .script(&path, [fake::Answer::bytes("image/jpeg", bytes)]);
        let input = Input::Image {
            message_id: message_id.to_owned(),
        };
        self.sends(self.admin, input).await.expect("an answer")
    }

    /// The admin writes `text` from "ประกาศ" and goes without a photo: the preview.
    async fn writes(&self, text: &str) -> Vec<Message> {
        self.taps(self.admin, "announcements").await;
        self.types(text).await;
        self.types("ข้าม").await
    }

    async fn in_flow(&self, user_id: i64) -> bool {
        session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .is_some()
    }

    async fn announcements(&self) -> Vec<announcement::Model> {
        announcement::Entity::find()
            .order_by_asc(announcement::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
    }

    /// Every image, as (owner type, owner).
    async fn images(&self) -> Vec<(OwnerType, i64)> {
        image::Entity::find()
            .order_by_asc(image::Column::CreatedAt)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|image| (image.owner_type, image.owner_id))
            .collect()
    }

    /// Every push queued, as (type, recipient, messages), oldest first.
    async fn pushes(&self) -> Vec<(String, String, Option<Json>)> {
        push_log::Entity::find()
            .order_by_asc(push_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.push_type, row.recipient, row.messages))
            .collect()
    }

    /// Every audit entry, as (actor, action, target type, target, details).
    async fn audited(&self) -> Vec<(Option<i64>, String, String, Option<i64>, Json)> {
        audit_log::Entity::find()
            .order_by_asc(audit_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.actor_id,
                    row.action,
                    row.target_type,
                    row.target_id,
                    row.details,
                )
            })
            .collect()
    }

    /// What the preview's confirm card says, with `pushed` this month in the log, LINE having
    /// said `quota`.
    fn costs(&self, recipients: u64, quota: Option<Quota>, pushed: u64) -> String {
        let confirm = Action::postback(labels::SEND_ANNOUNCEMENT.clone(), "confirm");
        let edit = Postback::new(EDIT).button(labels::EDIT.clone());
        let card = self
            .templates
            .confirm_announcement(recipients, quota, pushed, confirm, edit);
        text(&card).to_owned()
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

fn quick_replies(message: &Message) -> Vec<&str> {
    let Some(quick_reply) = &message.quick_reply else {
        return Vec::new();
    };
    quick_reply
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
}

/// The data of the confirm button among `message`'s Quick Replies.
fn confirm_button(message: &Message) -> String {
    let items = &message.quick_reply.as_ref().expect("a Quick Reply").items;
    items
        .iter()
        .find_map(|item| match &item.action {
            Action::Postback { data, .. } if data.starts_with(pending::CONFIRM) => {
                Some(data.clone())
            }
            _ => None,
        })
        .expect("a confirm button")
}

/// The messages of one push, as queued.
fn queued(messages: &[Message]) -> Option<Json> {
    Some(serde_json::to_value(messages).unwrap())
}

/// FR-ADM-07, NFR-SEC-02: only an admin announces, and one who loses the role mid-flow can't
/// go on.
#[tokio::test]
async fn only_admins_announce() {
    let s = setup().await;
    let t = &s.templates;

    let denied = s.taps(s.buyer, "announcements").await;
    assert_eq!(denied, [t.denied(Denied::NotAllowed)]);
    assert_eq!(s.taps(s.buyer, EDIT).await, denied);
    assert!(!s.in_flow(s.buyer).await);

    s.taps(s.admin, "announcements").await;
    user::Entity::update_many()
        .col_expr(user::Column::IsAdmin, Expr::value(false))
        .filter(user::Column::Id.eq(s.admin))
        .exec(&s.db)
        .await
        .unwrap();
    assert_eq!(s.types(TEXT).await, [t.denied(Denied::NotAllowed)]);
    assert!(!s.in_flow(s.admin).await);
    assert!(s.announcements().await.is_empty());
}

/// FR-ADM-07 acceptance: the preview shows the announcement as recipients will see it and what
/// it costs, and nothing is sent, queued or audited before the admin confirms.
#[tokio::test]
async fn nothing_is_sent_before_the_admin_confirms() {
    let s = setup().await;
    let t = &s.templates;

    let ask = s.taps(s.admin, "announcements").await;
    assert_eq!(
        ask,
        [templates::with_cancel(t.ask_announcement_text(TEXT_CHARS))]
    );
    let photo = s.types(TEXT).await;
    assert_eq!(text(&photo[0]), text(&t.ask_announcement_photo()));
    assert_eq!(
        quick_replies(&photo[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );

    let preview = s.types("ข้าม").await;
    assert_eq!(preview.len(), 2);
    assert_eq!(preview[0], t.announcement(TEXT));
    // The buyer, the suspended user and the other admin; LINE didn't say what is left
    assert_eq!(text(&preview[1]), s.costs(3, None, 0));
    assert_eq!(quick_replies(&preview[1]), ["ยืนยันส่ง", "แก้ไข", "ยกเลิก"]);
    assert!(!s.in_flow(s.admin).await);

    assert!(s.pushes().await.is_empty());
    assert!(s.audited().await.is_empty());
    let saved = s.announcements().await;
    assert_eq!(saved.len(), 1);
    assert_eq!(
        (
            saved[0].admin_id,
            saved[0].text.as_str(),
            saved[0].sent_at,
            saved[0].recipients
        ),
        (s.admin, TEXT, None, None)
    );
}

/// FR-ADM-07 acceptance, FR-NOT-02, FR-NOT-04, FR-ADM-08: once confirmed, the announcement and
/// its photo go in one push to each follower, suspended or not and admins too, but not to the
/// admin who sent it, a user who blocked the OA or a deleted account; sending it is audited.
#[tokio::test]
async fn only_followers_receive_it() {
    let s = setup().await;
    let t = &s.templates;
    s.taps(s.admin, "announcements").await;
    s.types(TEXT).await;
    let preview = s.sends_photo("m1", PHOTO).await;

    let saved = s.announcements().await;
    let id = saved[0].id;
    assert_eq!(s.images().await, [(OwnerType::Announcement, id)]);
    let photo = image::Entity::find().one(&s.db).await.unwrap().unwrap();
    let url = images::product_image_url(&s.config.app.base_url, photo.id);
    let announcement = [t.announcement(TEXT), Message::image(&url, &url)];
    assert_eq!(preview[..2], announcement);
    assert!(s.pushes().await.is_empty());

    let sending = s.taps(s.admin, &confirm_button(&preview[2])).await;
    assert_eq!(sending, [t.announcement_sending(3)]);

    let expected: Vec<_> = [BUYER, SUSPENDED, OTHER_ADMIN]
        .into_iter()
        .map(|to| {
            (
                "announcement".to_owned(),
                to.to_owned(),
                queued(&announcement),
            )
        })
        .collect();
    assert_eq!(s.pushes().await, expected);
    let sent = &s.announcements().await[0];
    assert_eq!((sent.sent_at, sent.recipients), (Some(t0()), Some(3)));
    assert_eq!(
        s.audited().await,
        [(
            Some(s.admin),
            "announcement.sent".to_owned(),
            "announcement".to_owned(),
            Some(id),
            json!({ "recipients": 3, "photo": true }),
        )]
    );
}

/// FR-UI-13: a second tap of the confirm button sends nothing more, and says when it went.
#[tokio::test]
async fn a_second_tap_sends_nothing_more() {
    let s = setup().await;
    let t = &s.templates;
    let preview = s.writes(TEXT).await;
    let confirm = confirm_button(&preview[1]);
    s.taps(s.admin, &confirm).await;
    let later = t0() + TimeDelta::minutes(5);

    let again = s.taps_at(s.admin, &confirm, later).await;

    assert_eq!(again, [t.announcement_sent_already(t0(), 3)]);
    assert_eq!(s.pushes().await.len(), 3);
    assert_eq!(s.audited().await.len(), 1);
}

/// FR-UI-13: "แก้ไข" writes the announcement anew, and the older preview's button sends
/// nothing; nor does that of a preview cancelled, or one whose button expired.
#[tokio::test]
async fn an_older_cancelled_or_expired_preview_sends_nothing() {
    let s = setup().await;
    let t = &s.templates;
    let gone = || {
        let again = Item::Announcements.postback();
        vec![t.announcement_gone(again.button(labels::ANNOUNCEMENTS.clone()))]
    };

    let older = s.writes("ประกาศฉบับแรก").await;
    let edit = s.taps(s.admin, EDIT).await;
    assert_eq!(
        edit,
        [templates::with_cancel(t.ask_announcement_text(TEXT_CHARS))]
    );
    // Starting anew deletes the older one
    assert!(s.announcements().await.is_empty());
    assert_eq!(s.taps(s.admin, &confirm_button(&older[1])).await, gone());

    let preview = s.types(TEXT).await;
    assert_eq!(text(&preview[0]), text(&t.ask_announcement_photo()));
    let cancelled = s.types("ข้าม").await;
    s.handle(s.admin, async |turn| flow::cancel(turn).await)
        .await;
    assert_eq!(
        s.taps(s.admin, &confirm_button(&cancelled[1])).await,
        gone()
    );

    let expired = s.writes(TEXT).await;
    let late = t0() + TimeDelta::minutes(SESSION.pending_action_minutes.into());
    assert_eq!(
        s.taps_at(s.admin, &confirm_button(&expired[1]), late).await,
        gone()
    );

    assert!(s.pushes().await.is_empty());
    assert!(s.audited().await.is_empty());
    assert!(
        s.announcements()
            .await
            .iter()
            .all(|announcement| announcement.sent_at.is_none())
    );
}

/// NFR-COST-01: the preview says how much of LINE's push quota is left, and warns when it is
/// not enough for every follower.
#[tokio::test]
async fn the_preview_tells_what_sending_costs() {
    let s = setup().await;
    let quota = |limit: Json, used: u64| {
        s.line.script(fake::QUOTA, [fake::Answer::json(200, limit)]);
        let used = json!({ "totalUsage": used });
        s.line
            .script(fake::QUOTA_CONSUMPTION, [fake::Answer::json(200, used)]);
    };

    quota(json!({ "type": "limited", "value": 300 }), 120);
    let enough = s.writes(TEXT).await;
    let left = Quota {
        limit: Some(300),
        used: 120,
    };
    assert_eq!(text(&enough[1]), s.costs(3, Some(left), 0));
    assert!(!text(&enough[1]).contains("โควตาไม่พอ"));

    quota(json!({ "type": "limited", "value": 300 }), 298);
    let short = s.writes(TEXT).await;
    let left = Quota {
        limit: Some(300),
        used: 298,
    };
    assert_eq!(text(&short[1]), s.costs(3, Some(left), 0));
    assert!(text(&short[1]).contains("โควตาไม่พอ จะส่งได้ประมาณ 2 คน"));

    quota(json!({ "type": "none" }), 4000);
    let unlimited = s.writes(TEXT).await;
    let used = Quota {
        limit: None,
        used: 4000,
    };
    assert_eq!(text(&unlimited[1]), s.costs(3, Some(used), 0));

    // LINE doesn't answer: the pushes logged this month instead
    s.line.script(
        fake::QUOTA,
        [fake::Answer::error(500, "Internal Server Error")],
    );
    let logged = s.writes(TEXT).await;
    assert_eq!(text(&logged[1]), s.costs(3, None, 0));
}

/// FR-UI-03, FR-UI-14: the text can't be empty or too long, a photo sent for the text is
/// answered rather than listed as a product, and the photo step asks again for anything but a
/// photo or "ข้าม".
#[tokio::test]
async fn each_step_asks_again_for_what_it_does_not_take() {
    let s = setup().await;
    let t = &s.templates;
    s.taps(s.admin, "announcements").await;

    assert_eq!(
        s.types("  ").await,
        [templates::with_cancel(t.ask_announcement_text(TEXT_CHARS))]
    );
    assert_eq!(
        s.types(&"ก".repeat(TEXT_CHARS + 1)).await,
        [templates::with_cancel(t.text_too_long(TEXT_CHARS))]
    );
    let photo_first = s.sends_photo("m0", PHOTO).await;
    assert_eq!(
        photo_first,
        [templates::with_cancel(t.announcement_text_expected())]
    );
    let location = Input::Location(Location {
        latitude: 16.44,
        longitude: 102.83,
        address: None,
    });
    assert_eq!(s.sends(s.admin, location.clone()).await, None);
    assert!(s.images().await.is_empty());

    let longest = "ข".repeat(TEXT_CHARS);
    s.types(&format!("  {longest}\n")).await;
    let refused = s.types("ไม่มีรูป").await;
    assert_eq!(text(&refused[0]), text(&t.photo_expected(&labels::SKIP)));
    assert_eq!(
        quick_replies(&refused[0]),
        ["ถ่ายรูป", "เลือกรูป", "ข้าม", "ยกเลิก"]
    );
    let not_a_photo = s.sends_photo("m1", b"not a photo").await;
    assert_eq!(
        text(&not_a_photo[0]),
        t.image_rejected(Rejected::NotJpegOrPng)
    );
    assert_eq!(s.sends(s.admin, location).await, None);

    let preview = s.types("ข้าม").await;
    assert_eq!(preview[0], t.announcement(&longest));
    limits::check(&[t.announcement(&longest)]).unwrap();
}

/// With no one but the admin following the OA, nothing is saved or sent, and the photo is
/// dropped.
#[tokio::test]
async fn nobody_to_send_to_sends_nothing() {
    let s = setup().await;
    let t = &s.templates;
    user::Entity::update_many()
        .col_expr(user::Column::IsFollowing, Expr::value(false))
        .filter(user::Column::Id.ne(s.admin))
        .exec(&s.db)
        .await
        .unwrap();
    s.taps(s.admin, "announcements").await;
    s.types(TEXT).await;

    let reply = s.sends_photo("m1", PHOTO).await;

    assert_eq!(reply[0], t.no_announcement_recipients());
    assert_eq!(reply.len(), 2);
    assert!(!s.in_flow(s.admin).await);
    assert!(s.announcements().await.is_empty());
    assert!(s.images().await.is_empty());
    assert!(s.pushes().await.is_empty());
}

/// §5.4: an announcement never confirmed is deleted with its photo once its button expires;
/// one sent, with the audit log.
#[tokio::test]
async fn announcements_are_deleted_with_their_photos() {
    let s = setup().await;
    s.taps(s.admin, "announcements").await;
    s.types(TEXT).await;
    let preview = s.sends_photo("m1", PHOTO).await;
    s.taps(s.admin, &confirm_button(&preview[2])).await;
    s.taps(s.admin, "announcements").await;
    s.types("ยังไม่ส่ง").await;
    s.sends_photo("m2", PHOTO).await;
    let saved = s.announcements().await;
    let [sent, unsent] = &saved[..] else {
        panic!("two announcements");
    };
    assert_eq!(s.images().await.len(), 2);

    // Their buttons last until t0 + 60 minutes
    let expiry = t0() + TimeDelta::minutes(SESSION.pending_action_minutes.into());
    assert_eq!(delete_unsent(&s.db, t0()).await.unwrap(), 0);
    assert_eq!(delete_unsent(&s.db, expiry).await.unwrap(), 1);
    assert_eq!(s.images().await, [(OwnerType::Announcement, sent.id)]);
    assert!(
        !s.announcements()
            .await
            .iter()
            .any(|announcement| announcement.id == unsent.id)
    );

    assert_eq!(delete_sent(&s.db, t0()).await.unwrap(), 0);
    let after = t0() + TimeDelta::seconds(1);
    assert_eq!(delete_sent(&s.db, after).await.unwrap(), 1);
    assert!(s.announcements().await.is_empty());
    assert!(s.images().await.is_empty());
}
