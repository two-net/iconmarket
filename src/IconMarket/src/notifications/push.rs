//! Push notifications (§4.8): the notifications of Table 4.8-A, pushed only to users who follow
//! the OA, in as few requests as LINE allows, each logged.
//!
//! A processing step, such as the handling of one event, collects what it notifies in an
//! [`Outbox`]. Just before the step's transaction commits, [`Outbox::queue`] drops what the
//! switches turn off (FR-NOT-01) and what would go to users not following the OA (FR-NOT-02),
//! merges each recipient's messages into as few push requests as LINE takes (FR-NOT-04), and
//! writes each request to the push log as `PENDING`, with its messages and a retry key. A step
//! that is rolled back therefore notifies no one. Once it has committed, [`send`] pushes the
//! requests and records each result (FR-NOT-05). What is left `PENDING`, by a failure or by a
//! crash, is sent by the retry job ([`retry`]) with the same retry key, so LINE delivers it once
//! (FR-LINE-12).
//!
//! Pushes count against the OA's monthly quota, so only these notifications are pushed, and no
//! marketing but the admins' announcements (NFR-COST-01).

#![allow(
    dead_code,
    reason = "the features that notify are built by later issues"
)]

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Months, NaiveTime, TimeDelta, Utc};
use chrono_tz::Tz;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::config::NotificationConfig;
use crate::line::limits::{self, MESSAGES_PER_REQUEST};
use crate::line::{self, Message, Pushed, RetryKey};
use crate::persistence::entity::push_log::{self, Status};
use crate::persistence::entity::user;
use crate::thai;

/// How long the retry job waits before sending a push again, by the send calls made so far: a
/// push never sent is left a minute for the step that queued it to send it, and a push that
/// failed after the last wait is given up.
const RETRY_AFTER: [TimeDelta; 5] = [
    TimeDelta::minutes(1),
    TimeDelta::minutes(5),
    TimeDelta::minutes(15),
    TimeDelta::hours(1),
    TimeDelta::hours(3),
];

/// How long after it was queued a push may still be sent. LINE remembers a retry key for 24
/// hours, so a push sent later could be delivered twice.
const RETRY_WINDOW: TimeDelta = TimeDelta::hours(23);

/// Pushes the retry job sends per run; any more are sent by the next
const RETRIES_PER_RUN: u64 = 100;

/// A notification of Table 4.8-A. Each feature builds its own, with buttons for what the
/// recipient can do about it (FR-NOT-03), using
/// [`templates::notice`](super::templates::notice). The daily summary, the village group digest
/// and announcements are added by #47, #48 and #49.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// To the seller: a new order (FR-ORD-03)
    NewOrder,
    /// To the buyer: their order's status changed (FR-ORD-08)
    OrderStatus,
    /// To the seller: the buyer cancelled an order, or an admin did (FR-ORD-12)
    OrderCancelled,
    /// To the seller: an order expired unanswered (FR-ORD-05)
    OrderExpired,
    /// To the seller: the buyer asks to cancel an accepted order (FR-ORD-07)
    CancellationRequested,
    /// To the seller: a payment slip (FR-PAY-05)
    SlipReceived,
    /// To the buyer: their PromptPay order was accepted, with what to transfer and where
    /// (FR-PAY-03)
    PaymentDetails,
    /// To the seller: the buyer received an order not yet paid; was it? (FR-PAY-07)
    PaymentToConfirm,
    /// To the applicant: their shop was approved
    ShopApproved,
    /// To the applicant: their shop was rejected
    ShopRejected,
    /// To the seller: their product or shop was held for review. Moderation holds it while
    /// the seller is changing it, so tells them in the reply instead (FR-LINE-07).
    ItemHeld,
    /// To the seller: their product was removed, with why (FR-ADM-03)
    ItemRemoved,
    /// To the owner: their shop was suspended, with why (FR-ADM-03)
    ShopSuspended,
    /// To the owner: their shop's suspension was lifted (FR-ADM-04)
    ShopUnsuspended,
    /// To the user: their account was suspended, with why (FR-ADM-04)
    AccountSuspended,
    /// To the user: their account's suspension was lifted (FR-ADM-04)
    AccountUnsuspended,
    /// To the admins: a shop application to decide on
    ShopApplication,
    /// To the admins: a product or shop held for review, or a shop's application to look at
    /// closely, by moderation (FR-PROD-09)
    HeldForReview,
    /// To the admins: a user reported a problem
    UserReport,
}

impl Kind {
    /// Its name in `push_logs.push_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewOrder => "order.new",
            Self::OrderStatus => "order.status",
            Self::OrderCancelled => "order.cancelled",
            Self::OrderExpired => "order.expired",
            Self::CancellationRequested => "order.cancellation_requested",
            Self::SlipReceived => "payment.slip",
            Self::PaymentDetails => "payment.promptpay",
            Self::PaymentToConfirm => "payment.to_confirm",
            Self::ShopApproved => "shop.approved",
            Self::ShopRejected => "shop.rejected",
            Self::ItemHeld => "product.held",
            Self::ItemRemoved => "product.removed",
            Self::ShopSuspended => "shop.suspended",
            Self::ShopUnsuspended => "shop.unsuspended",
            Self::AccountSuspended => "user.suspended",
            Self::AccountUnsuspended => "user.unsuspended",
            Self::ShopApplication => "admin.shop_application",
            Self::HeldForReview => "admin.held_for_review",
            Self::UserReport => "admin.user_report",
        }
    }

    /// Whether it is sent (FR-NOT-01): by its switch, or always for those Table 4.8-A gives none.
    pub fn is_on(self, config: &NotificationConfig) -> bool {
        match self {
            Self::NewOrder => config.new_order_to_seller,
            Self::OrderStatus => config.order_status_to_buyer,
            Self::ShopApplication | Self::HeldForReview | Self::UserReport => config.admin_alerts,
            Self::OrderCancelled
            | Self::OrderExpired
            | Self::CancellationRequested
            | Self::SlipReceived
            | Self::PaymentDetails
            | Self::PaymentToConfirm
            | Self::ShopApproved
            | Self::ShopRejected
            | Self::ItemHeld
            | Self::ItemRemoved
            | Self::ShopSuspended
            | Self::ShopUnsuspended
            | Self::AccountSuspended
            | Self::AccountUnsuspended => true,
        }
    }
}

/// Who a notification is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum To {
    /// The user with this `users.id`
    User(i64),
    /// Every admin
    Admins,
}

/// One notification, as added.
#[derive(Debug)]
struct Notice {
    kind: Kind,
    to: To,
    messages: Vec<Message>,
}

/// The notifications of one processing step, sent once the step takes effect.
#[derive(Debug, Default)]
pub struct Outbox {
    notices: Vec<Notice>,
}

impl Outbox {
    /// Adds a notification of `kind` for `to`: up to five messages, which go out in the same
    /// request. Its buttons go in the messages themselves, as LINE shows a Quick Reply only
    /// under the last message of a request. Messages that break one of LINE's limits are a bug:
    /// they are logged and dropped, so that the step still takes effect.
    pub fn add(&mut self, kind: Kind, to: To, messages: Vec<Message>) {
        if let Err(e) = limits::check(&messages) {
            error!(
                kind = kind.as_str(),
                error = %e,
                "the notification breaks LINE's limits, so it is not sent"
            );
            return;
        }
        self.notices.push(Notice { kind, to, messages });
    }

    pub fn is_empty(&self) -> bool {
        self.notices.is_empty()
    }

    /// Queues the notifications in `db`, the step's transaction, at `now`: those switched on
    /// (FR-NOT-01), for recipients who follow the OA (FR-NOT-02), each recipient's in as few
    /// requests as possible, in the order added (FR-NOT-04). A notification's messages stay in
    /// one request. Returns the requests to [`send`] once the transaction commits.
    pub async fn queue(
        self,
        db: &impl ConnectionTrait,
        config: &NotificationConfig,
        now: DateTimeUtc,
    ) -> Result<Queued, DbErr> {
        let notices: Vec<Notice> = self
            .notices
            .into_iter()
            .filter(|notice| {
                let on = notice.kind.is_on(config);
                if !on {
                    debug!(kind = notice.kind.as_str(), "switched off; not pushed");
                }
                on
            })
            .collect();
        if notices.is_empty() {
            return Ok(Queued::default());
        }

        let followers = Followers::of(db, &notices).await?;
        let mut requests: Vec<Request> = Vec::new();
        for notice in &notices {
            let recipients = followers.of_notice(notice.to);
            if recipients.is_empty() {
                // Such as a seller who blocked the OA (AC-17)
                debug!(
                    kind = notice.kind.as_str(),
                    "no recipient follows the OA; not pushed"
                );
            }
            for recipient in recipients {
                add_to_requests(&mut requests, recipient, notice);
            }
        }
        if requests.is_empty() {
            return Ok(Queued::default());
        }

        let rows = requests.into_iter().map(|request| {
            let push_type: Vec<_> = request.kinds.iter().map(|kind| kind.as_str()).collect();
            let messages = serde_json::to_value(&request.messages).expect("messages serialize");
            push_log::ActiveModel {
                push_type: Set(push_type.join(",")),
                recipient: Set(request.recipient.to_owned()),
                retry_key: Set(Some(RetryKey::random().uuid())),
                status: Set(Status::Pending),
                error: Set(None),
                sent_at: Set(now),
                messages: Set(Some(messages)),
                attempts: Set(0),
                queued_at: Set(now),
                ..Default::default()
            }
        });
        let mut pushes: Vec<Push> = push_log::Entity::insert_many(rows)
            .exec_with_returning(db)
            .await?
            .into_iter()
            .filter_map(Push::from_row)
            .collect();
        pushes.sort_by_key(|push| push.id);
        Ok(Queued(pushes))
    }
}

/// One push request being put together.
struct Request<'a> {
    recipient: &'a str,
    kinds: Vec<Kind>,
    messages: Vec<Message>,
}

/// Adds `notice` to the last request for `recipient` if it has room, or else to a new one.
fn add_to_requests<'a>(requests: &mut Vec<Request<'a>>, recipient: &'a str, notice: &Notice) {
    let room = requests
        .iter_mut()
        .rev()
        .find(|request| request.recipient == recipient)
        .filter(|request| request.messages.len() + notice.messages.len() <= MESSAGES_PER_REQUEST);
    let request = match room {
        Some(request) => request,
        None => {
            requests.push(Request {
                recipient,
                kinds: Vec::new(),
                messages: Vec::new(),
            });
            requests.last_mut().expect("just added")
        }
    };
    if !request.kinds.contains(&notice.kind) {
        request.kinds.push(notice.kind);
    }
    request.messages.extend(notice.messages.iter().cloned());
}

/// The LINE user IDs of the recipients of a step's notifications who follow the OA.
struct Followers {
    /// By `users.id`
    users: HashMap<i64, String>,
    /// In ID order
    admins: Vec<String>,
}

impl Followers {
    async fn of(db: &impl ConnectionTrait, notices: &[Notice]) -> Result<Self, DbErr> {
        let ids: HashSet<i64> = notices
            .iter()
            .filter_map(|notice| match notice.to {
                To::User(id) => Some(id),
                To::Admins => None,
            })
            .collect();
        let mut wanted = Condition::any().add(user::Column::Id.is_in(ids));
        if notices.iter().any(|notice| notice.to == To::Admins) {
            wanted = wanted.add(user::Column::IsAdmin.eq(true));
        }
        // A deleted account has no LINE user ID (FR-ONB-07)
        let rows: Vec<(i64, String, bool)> = user::Entity::find()
            .select_only()
            .columns([
                user::Column::Id,
                user::Column::LineUserId,
                user::Column::IsAdmin,
            ])
            .filter(wanted)
            .filter(user::Column::IsFollowing.eq(true))
            .filter(user::Column::LineUserId.is_not_null())
            .order_by_asc(user::Column::Id)
            .into_tuple()
            .all(db)
            .await?;
        let admins = rows
            .iter()
            .filter(|(_, _, is_admin)| *is_admin)
            .map(|(_, line_user_id, _)| line_user_id.clone())
            .collect();
        let users = rows
            .into_iter()
            .map(|(id, line_user_id, _)| (id, line_user_id))
            .collect();
        Ok(Self { users, admins })
    }

    fn of_notice(&self, to: To) -> Vec<&str> {
        match to {
            To::User(id) => self
                .users
                .get(&id)
                .map(String::as_str)
                .into_iter()
                .collect(),
            To::Admins => self.admins.iter().map(String::as_str).collect(),
        }
    }
}

/// A push request in the push log, ready to send.
#[derive(Clone, Debug, PartialEq)]
struct Push {
    id: i64,
    recipient: String,
    messages: Value,
    retry_key: RetryKey,
    /// Send calls made so far
    attempts: i32,
}

impl Push {
    /// `None` for a row with nothing left to send.
    fn from_row(row: push_log::Model) -> Option<Self> {
        Some(Self {
            id: row.id,
            recipient: row.recipient,
            messages: row.messages?,
            retry_key: row.retry_key?.into(),
            attempts: row.attempts,
        })
    }
}

/// The push requests a step queued, in order, to [`send`] once it commits.
#[derive(Debug, Default)]
pub struct Queued(Vec<Push>);

impl Queued {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Sends the pushes a step queued, once it has committed, one after another, and records each
/// result in the push log (FR-NOT-05). A push that fails in a way worth retrying, or whose result
/// can't be recorded, stays `PENDING` for the retry job.
pub async fn send(db: &DatabaseConnection, line: &line::Client, queued: Queued) {
    for push in queued.0 {
        let result = line
            .push_stored(&push.recipient, &push.messages, push.retry_key)
            .await;
        if let Err(e) = record(db, &push, &result, Utc::now()).await {
            warn!(push = push.id, error = %e, "cannot record a push; the retry job sends it again");
        }
    }
}

/// What the push log calls an essential reply pushed instead (FR-LINE-07)
const REPLY: &str = "reply";

/// Logs the push of an essential reply to `recipient`, made with `retry_key` once its reply
/// token failed (FR-LINE-07, FR-NOT-05), and how it went. One that failed in a way worth
/// retrying stays `PENDING` with its `messages`, so the retry job sends it again with the same
/// retry key. Returns its status.
pub async fn log_reply_pushed(
    db: &DatabaseConnection,
    recipient: &str,
    messages: &[Message],
    retry_key: RetryKey,
    result: &Result<Pushed, line::Error>,
) -> Result<Status, DbErr> {
    let now = Utc::now();
    let row = push_log::ActiveModel {
        push_type: Set(REPLY.to_owned()),
        recipient: Set(recipient.to_owned()),
        retry_key: Set(Some(retry_key.uuid())),
        status: Set(Status::Pending),
        error: Set(None),
        sent_at: Set(now),
        messages: Set(Some(
            serde_json::to_value(messages).expect("messages serialize"),
        )),
        attempts: Set(0),
        queued_at: Set(now),
        ..Default::default()
    };
    let row = push_log::Entity::insert(row)
        .exec_with_returning(db)
        .await?;
    let push = Push::from_row(row).expect("its messages and retry key were just stored");
    record(db, &push, result, now).await
}

/// What a run of the retry job did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retried {
    pub sent: usize,
    /// Failed again, and tried again later
    pub pending: usize,
    /// Given up
    pub failed: usize,
}

/// The retry job's work (FR-JOB-02, FR-LINE-12): sends at `now` the pushes due again, oldest
/// first, each with its own retry key, so a push already delivered isn't delivered again. A push
/// is given up when too late for its retry key, or when its recipient no longer follows the OA
/// (FR-NOT-02).
pub async fn retry(
    db: &DatabaseConnection,
    line: &line::Client,
    now: DateTimeUtc,
) -> Result<Retried, DbErr> {
    let mut due = Condition::any().add(push_log::Column::QueuedAt.lte(now - RETRY_WINDOW));
    for (attempts, wait) in (0..).zip(RETRY_AFTER) {
        due = due.add(
            Condition::all()
                .add(push_log::Column::Attempts.eq(attempts))
                .add(push_log::Column::SentAt.lte(now - wait)),
        );
    }
    let rows = push_log::Entity::find()
        .filter(push_log::Column::Status.eq(Status::Pending))
        .filter(due)
        .order_by_asc(push_log::Column::Id)
        .limit(RETRIES_PER_RUN)
        .all(db)
        .await?;
    if rows.is_empty() {
        return Ok(Retried::default());
    }

    let recipients: HashSet<&str> = rows.iter().map(|row| row.recipient.as_str()).collect();
    let following: HashSet<String> = user::Entity::find()
        .select_only()
        .column(user::Column::LineUserId)
        .filter(user::Column::LineUserId.is_in(recipients))
        .filter(user::Column::IsFollowing.eq(true))
        .into_tuple()
        .all(db)
        .await?
        .into_iter()
        .collect();

    let mut retried = Retried::default();
    for row in rows {
        let id = row.id;
        let too_late = row.queued_at <= now - RETRY_WINDOW;
        let follows = following.contains(&row.recipient);
        let status = match Push::from_row(row) {
            Some(push) if !too_late && follows => {
                let result = line
                    .push_stored(&push.recipient, &push.messages, push.retry_key)
                    .await;
                record(db, &push, &result, now).await?
            }
            _ => {
                let why = if too_late {
                    "too late to send it only once"
                } else if !follows {
                    "the recipient no longer follows the OA"
                } else {
                    "nothing to send"
                };
                give_up(db, id, why, now).await?;
                Status::Failed
            }
        };
        match status {
            Status::Sent => retried.sent += 1,
            Status::Pending => retried.pending += 1,
            Status::Failed => retried.failed += 1,
        }
    }
    Ok(retried)
}

/// Records how a send call at `now` of `push` went, unless the push is no longer `PENDING`.
/// Returns its status now.
async fn record(
    db: &DatabaseConnection,
    push: &Push,
    result: &Result<Pushed, line::Error>,
    now: DateTimeUtc,
) -> Result<Status, DbErr> {
    let attempts = push.attempts + 1;
    let (status, error) = match result {
        Ok(pushed) => {
            debug!(push = push.id, attempts, tries = pushed.attempts, "pushed");
            (Status::Sent, None)
        }
        Err(e)
            if e.is_retryable()
                && usize::try_from(attempts).unwrap_or(usize::MAX) < RETRY_AFTER.len() =>
        {
            warn!(push = push.id, attempts, error = %e, "push failed; the retry job tries again");
            (Status::Pending, Some(e.to_string()))
        }
        Err(e) => {
            warn!(push = push.id, attempts, error = %e, "push failed; given up");
            (Status::Failed, Some(e.to_string()))
        }
    };
    let mut update = push_log::Entity::update_many()
        .col_expr(push_log::Column::Status, Expr::value(status))
        .col_expr(push_log::Column::Error, Expr::value(error))
        .col_expr(push_log::Column::Attempts, Expr::value(attempts))
        .col_expr(push_log::Column::SentAt, Expr::value(now));
    if status != Status::Pending {
        update = update.col_expr(
            push_log::Column::Messages,
            Expr::value(Option::<Value>::None),
        );
    }
    update
        .filter(push_log::Column::Id.eq(push.id))
        .filter(push_log::Column::Status.eq(Status::Pending))
        .exec(db)
        .await?;
    Ok(status)
}

/// Fails push `id` for good at `now`, without sending it, for reason `why`.
async fn give_up(
    db: &DatabaseConnection,
    id: i64,
    why: &str,
    now: DateTimeUtc,
) -> Result<(), DbErr> {
    warn!(push = id, why, "push given up");
    push_log::Entity::update_many()
        .col_expr(push_log::Column::Status, Expr::value(Status::Failed))
        .col_expr(
            push_log::Column::Error,
            Expr::value(format!("given up: {why}")),
        )
        .col_expr(push_log::Column::SentAt, Expr::value(now))
        .col_expr(
            push_log::Column::Messages,
            Expr::value(Option::<Value>::None),
        )
        .filter(push_log::Column::Id.eq(id))
        .filter(push_log::Column::Status.eq(Status::Pending))
        .exec(db)
        .await?;
    Ok(())
}

/// Pushes this month, for the admin statistics (FR-NOT-05, FR-ADM-06).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushStats {
    pub sent: u64,
    /// Given up, such as when the monthly quota was used up
    pub failed: u64,
}

/// The pushes sent and given up in the month of `now`, in the business time zone.
pub async fn stats(
    db: &impl ConnectionTrait,
    now: DateTimeUtc,
    time_zone: Tz,
) -> Result<PushStats, DbErr> {
    let first = now
        .with_timezone(&time_zone)
        .date_naive()
        .with_day(1)
        .expect("every month has a first day");
    let next = first
        .checked_add_months(Months::new(1))
        .expect("a date before the year 262143");
    let from = thai::local_instant(first, NaiveTime::MIN, time_zone);
    let until = thai::local_instant(next, NaiveTime::MIN, time_zone);
    let count = |status: Status| {
        push_log::Entity::find()
            .filter(push_log::Column::Status.eq(status))
            .filter(push_log::Column::SentAt.gte(from))
            .filter(push_log::Column::SentAt.lt(until))
            .count(db)
    };
    Ok(PushStats {
        sent: count(Status::Sent).await?,
        failed: count(Status::Failed).await?,
    })
}
