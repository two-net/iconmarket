//! The inbox worker (FR-LINE-05, FR-JOB-02): takes the webhook's stored events from the inbox
//! strictly in order for each LINE source, a user's chat or a group's, and concurrently across
//! sources.
//!
//! An event is handled in one transaction, which also marks it `DONE`, and answered once that
//! commits, so it takes effect at most once (NFR-REL-02). Its notifications, queued in the same
//! transaction, are pushed after the reply. Each attempt is counted before it
//! starts, in a transaction of its own, so one cut short by a crash or a shutdown counts too. A
//! failed attempt changes nothing and is tried again shortly; after [`MAX_ATTEMPTS`] the event
//! fails for good and its user is told something went wrong. The source's next event waits
//! until then, so order holds. An attempt holds one database connection at a time, so sources
//! handled at once can't hold the pool between them while each waits for another.
//!
//! The webhook announces each stored request ([`Wakeup`]), and the worker also looks at the
//! inbox every few seconds, so that events stored by another instance, or left when the app
//! stopped, are handled too (NFR-REL-03). Counting an attempt and handling the event each hold
//! a database lock on the source until their transaction ends, so however many instances there
//! are, each source is handled by one at a time, in order (FR-JOB-04).

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr,
    EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Instrument, Span, debug, error, info, warn};

use super::event::{Event, Source};
use super::route::{self, Handled, Reply};
use crate::config::Config;
use crate::line::{self, Delivery, Importance, Message};
use crate::notifications::templates::Templates;
use crate::notifications::{self, Queued};
use crate::persistence::entity::webhook_event::{self, Status};
use crate::{ai, logging};

/// Attempts at an event before it fails for good
pub const MAX_ATTEMPTS: i32 = 3;

/// The class of the inbox's advisory locks, a key space of their own: a source's lock is
/// `(LOCK_CLASS, hashtext(user_key))`
const LOCK_CLASS: i32 = 0x1b0c;

/// The events still to handle, cut short or not
const WAITING: [Status; 2] = [Status::Pending, Status::Processing];

/// Sources taken from the inbox per look; any more are found by the next
const SOURCES_PER_LOOK: u64 = 256;

/// How soon things happen.
#[derive(Clone, Copy, Debug)]
struct Pace {
    /// Waited before the second attempt at an event, and before the third
    retry_after: [Duration; 2],
    /// How often the inbox is looked at without a wakeup
    look_every: Duration,
}

const PACE: Pace = Pace {
    retry_after: [Duration::from_secs(1), Duration::from_secs(2)],
    look_every: Duration::from_secs(5),
};

/// What handling events works with.
pub struct Context {
    pub db: DatabaseConnection,
    pub line: line::Client,
    pub ai: ai::Gateway,
    pub templates: Templates,
    pub config: Arc<Config>,
}

/// Wakes the worker when the webhook has stored events. Cheap to clone: clones wake the same
/// worker.
#[derive(Clone, Debug, Default)]
pub struct Wakeup(Arc<Notify>);

impl Wakeup {
    /// Wakes the worker now or, if it is busy, as soon as it next waits.
    pub fn notify(&self) {
        self.0.notify_one();
    }
}

/// Starts the worker on `tracker`. It handles what the inbox holds now, then whatever `wakeup`
/// announces or the next look finds, at most half `database.maxConnections` sources at a time,
/// so the webhook, the image links and the jobs keep connections. Once `stop` is cancelled it
/// takes no more events: those in hand are finished, and the rest stay in the inbox for the
/// next start (NFR-REL-05).
pub fn start(tracker: &TaskTracker, stop: CancellationToken, context: Context, wakeup: Wakeup) {
    let sources = usize::try_from(context.config.database.max_connections / 2)
        .unwrap_or(1)
        .max(1);
    Worker::new(tracker, stop, context, wakeup, sources, PACE).start();
    info!(sources, "inbox worker started");
}

struct Worker {
    context: Context,
    wakeup: Wakeup,
    stop: CancellationToken,
    tracker: TaskTracker,
    /// One per event being handled, shared by the sources, so that they share the database pool
    permits: Semaphore,
    /// The sources being handled here, each by a task of its own
    busy: Mutex<HashSet<String>>,
    pace: Pace,
}

/// What [`claim`] found.
enum Claimed {
    /// The first waiting event, its attempt counted
    Event(webhook_event::Model),
    /// The first waiting event had no attempts left, so it was given up
    GivenUp,
    /// The source has no event waiting
    Empty,
    /// Another instance is handling the source
    Elsewhere,
}

/// What became of a try at a source's next event.
#[derive(Debug)]
enum Next {
    /// It was attempted; its answer, if any, goes out before the source's next is taken, so a
    /// recipient's notifications arrive in order
    Attempted(Option<Answer>),
    /// The attempt failed; try again after this long
    Retry(Duration),
    /// The source has no event waiting
    Empty,
    /// Another instance is handling the source
    Elsewhere,
}

/// What to send once an event is done with: a reply, notifications, or both.
#[derive(Debug)]
struct Answer {
    handled: Handled,
    /// When its event was received, to measure how long the reply took (NFR-PERF-02)
    received_at: DateTimeUtc,
    /// Its event's span
    span: Span,
}

impl Worker {
    fn new(
        tracker: &TaskTracker,
        stop: CancellationToken,
        context: Context,
        wakeup: Wakeup,
        sources: usize,
        pace: Pace,
    ) -> Arc<Self> {
        Arc::new(Self {
            context,
            wakeup,
            stop,
            tracker: tracker.clone(),
            permits: Semaphore::new(sources),
            busy: Mutex::new(HashSet::new()),
            pace,
        })
    }

    fn start(self: Arc<Self>) {
        let tracker = self.tracker.clone();
        tracker.spawn(self.run());
    }

    /// Looks at the inbox, then again whenever woken or after a while, until `stop`.
    async fn run(self: Arc<Self>) {
        while !self.stop.is_cancelled() {
            match waiting_sources(&self.context.db).await {
                Ok(sources) => {
                    for source in sources {
                        self.take(source);
                    }
                }
                Err(e) => warn!(error = %e, "cannot read the inbox; trying again shortly"),
            }
            tokio::select! {
                () = self.stop.cancelled() => {}
                () = self.wakeup.0.notified() => {}
                () = tokio::time::sleep(self.pace.look_every) => {}
            }
        }
    }

    /// Handles `source`'s events on a task of its own, unless one already is.
    fn take(self: &Arc<Self>, source: String) {
        if !self.busy().insert(source.clone()) {
            return;
        }
        let worker = Arc::clone(self);
        self.tracker.spawn(async move {
            let look_again = worker.drain(&source).await;
            worker.busy().remove(&source);
            // An event stored while it was finishing would otherwise wait for the next look
            if look_again {
                worker.wakeup.notify();
            }
        });
    }

    fn busy(&self) -> MutexGuard<'_, HashSet<String>> {
        self.busy.lock().expect("no panic while held")
    }

    /// Handles `source`'s events one after another, until none is waiting or `stop`. Returns
    /// whether to look at the inbox again straight away: not when another instance holds the
    /// source or the database is down, as that would only spin.
    async fn drain(self: &Arc<Self>, source: &str) -> bool {
        loop {
            let next = {
                let _permit = self.permits.acquire().await.expect("never closed");
                if self.stop.is_cancelled() {
                    return true;
                }
                self.next(source).await
            };
            match next {
                // Without a permit, so that LINE being slow doesn't hold up other sources
                Ok(Next::Attempted(Some(answer))) => self.send(answer).await,
                Ok(Next::Attempted(None)) => {}
                Ok(Next::Retry(wait)) => {
                    tokio::select! {
                        () = self.stop.cancelled() => return true,
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                Ok(Next::Empty) => return true,
                Ok(Next::Elsewhere) => return false,
                Err(e) => {
                    warn!(error = %e, "cannot take an event from the inbox; trying again shortly");
                    return false;
                }
            }
        }
    }

    /// Takes `source`'s first waiting event and makes an attempt at it.
    async fn next(self: &Arc<Self>, source: &str) -> Result<Next, DbErr> {
        let db = &self.context.db;
        let row = match claim(db, source).await? {
            Claimed::Event(row) => row,
            Claimed::GivenUp => return Ok(Next::Attempted(None)),
            Claimed::Empty => return Ok(Next::Empty),
            Claimed::Elsewhere => return Ok(Next::Elsewhere),
        };
        // The source's lock again, for as long as the event is handled. Another instance may
        // have taken the source in between: it then handles the event itself, having counted an
        // attempt of its own, so an event two instances raced for shows one attempt too many.
        let txn = db.begin().await?;
        if !lock(&txn, source).await? || !still_claimed(&txn, &row).await? {
            txn.rollback().await?;
            return Ok(Next::Elsewhere);
        }

        let span = logging::event_span(&row.event_id);
        let now = Utc::now();
        let worker = Arc::clone(self);
        let event = row.clone();
        // On a task of its own, so that a panic fails the attempt instead of the source's task
        let attempt = async move { worker.attempt(txn, &event, now).await };
        let result = match tokio::spawn(attempt.instrument(span.clone())).await {
            Ok(result) => result.map_err(|e| format!("{e:#}")),
            Err(e) => Err(e.to_string()),
        };
        let reply = self.settle(&row, result).instrument(span.clone()).await?;
        Ok(match reply {
            Settled::Answer(handled) => Next::Attempted(Some(Answer {
                handled,
                received_at: row.received_at,
                span,
            })),
            Settled::Silent => Next::Attempted(None),
            Settled::Retry(wait) => Next::Retry(wait),
        })
    }

    /// Handles the event `row` in `txn`, marks it `DONE` and commits. Returns the reply and the
    /// notifications to send.
    async fn attempt(
        &self,
        txn: DatabaseTransaction,
        row: &webhook_event::Model,
        now: DateTimeUtc,
    ) -> Result<Handled, DbErr> {
        let event = Event::read(&row.payload);
        let handled = route::handle(&self.context, &txn, row, event, now).await?;
        finish(&txn, row.id, Status::Done, None).await?;
        txn.commit().await?;
        Ok(handled)
    }

    /// What follows attempt `row.attempts` at `row`, which ended with `result`: what to send, or,
    /// recorded first, why it failed.
    async fn settle(
        &self,
        row: &webhook_event::Model,
        result: Result<Handled, String>,
    ) -> Result<Settled, DbErr> {
        let attempt = row.attempts;
        let error = match result {
            Ok(handled) => {
                debug!(event_type = %row.event_type, attempt, "event handled");
                return Ok(if handled.reply.is_none() && handled.pushes.is_empty() {
                    Settled::Silent
                } else {
                    Settled::Answer(handled)
                });
            }
            Err(error) => error,
        };
        if attempt < MAX_ATTEMPTS {
            failed(&self.context.db, row.id, attempt, Status::Pending, &error).await?;
            let wait = self.pace.retry_after[usize::from(attempt > 1)];
            let retry_in_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
            warn!(
                attempt,
                error, retry_in_ms, "event failed; trying again shortly"
            );
            return Ok(Settled::Retry(wait));
        }
        failed(&self.context.db, row.id, attempt, Status::Failed, &error).await?;
        warn!(attempt, error, "event failed; given up");
        // The only reply the event gets. Group events get none (FR-LINE-06).
        let event = Event::read(&row.payload);
        let (Some(token), Source::User(_)) = (event.reply_token, event.source) else {
            return Ok(Settled::Silent);
        };
        let sorry = self.context.templates.something_went_wrong();
        Ok(Settled::Answer(Handled {
            reply: Some(Reply {
                token,
                to: row.user_key.clone(),
                messages: vec![Message::text(sorry)],
            }),
            pushes: Queued::default(),
        }))
    }

    /// Sends `answer`: its reply first, while the reply token is fresh, then its notifications
    /// (FR-NOT-04).
    async fn send(&self, answer: Answer) {
        let Answer {
            handled: Handled { reply, pushes },
            received_at,
            span,
        } = answer;
        if let Some(reply) = reply {
            self.reply(reply, received_at, &span).await;
        }
        if !pushes.is_empty() {
            let count = pushes.len();
            notifications::send(&self.context.db, &self.context.line, pushes)
                .instrument(span.clone())
                .await;
            span.in_scope(|| debug!(pushes = count, "notifications sent"));
        }
    }

    /// Sends `reply` with its event's reply token, or not at all if that fails: a reply is never
    /// pushed instead unless it is essential (FR-LINE-07).
    async fn reply(&self, reply: Reply, received_at: DateTimeUtc, span: &Span) {
        let delivery = self
            .context
            .line
            .respond(
                Some(&reply.token),
                &reply.to,
                &reply.messages,
                Importance::Optional,
            )
            .instrument(span.clone())
            .await;
        let latency_ms = (Utc::now() - received_at).num_milliseconds();
        span.in_scope(|| match delivery {
            Ok(Delivery::Replied) => debug!(latency_ms, "replied"),
            Ok(delivery) => debug!(latency_ms, ?delivery, "reply not delivered"),
            Err(e) => error!(error = %e, "the reply breaks LINE's limits, so it was not sent"),
        });
    }
}

/// What [`Worker::settle`] made of an attempt.
enum Settled {
    /// Done with, with this to send
    Answer(Handled),
    /// Done with, with nothing to send
    Silent,
    /// To be tried again after this long
    Retry(Duration),
}

/// The sources with events waiting, those that have waited longest first.
async fn waiting_sources(db: &DatabaseConnection) -> Result<Vec<String>, DbErr> {
    webhook_event::Entity::find()
        .select_only()
        .column(webhook_event::Column::UserKey)
        .filter(webhook_event::Column::Status.is_in(WAITING))
        .group_by(webhook_event::Column::UserKey)
        .order_by(Func::min(Expr::col(webhook_event::Column::Id)), Order::Asc)
        .limit(SOURCES_PER_LOOK)
        .into_tuple()
        .all(db)
        .await
}

/// Takes the lock on `source` that `txn` then holds until it ends, unless another instance
/// holds it.
async fn lock(txn: &DatabaseTransaction, source: &str) -> Result<bool, DbErr> {
    let try_lock = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_try_advisory_xact_lock($1, hashtext($2))",
        [LOCK_CLASS.into(), source.into()],
    );
    let row = txn.query_one_raw(try_lock).await?;
    Ok(row.map(|row| row.try_get_by_index::<bool>(0)).transpose()? == Some(true))
}

/// Takes `source`'s first waiting event and counts an attempt at it, marking it `PROCESSING`,
/// in a transaction of its own so that an attempt cut short counts too. An event whose
/// attempts were all cut short is given up instead: it may be what stops the app.
async fn claim(db: &DatabaseConnection, source: &str) -> Result<Claimed, DbErr> {
    let txn = db.begin().await?;
    if !lock(&txn, source).await? {
        txn.rollback().await?;
        return Ok(Claimed::Elsewhere);
    }
    let Some(row) = first_waiting(&txn, source).await? else {
        txn.rollback().await?;
        return Ok(Claimed::Empty);
    };
    if row.attempts >= MAX_ATTEMPTS {
        give_up(&txn, &row).await?;
        txn.commit().await?;
        logging::event_span(&row.event_id).in_scope(|| {
            warn!(
                attempts = row.attempts,
                "event cut short at its last attempt; given up"
            );
        });
        return Ok(Claimed::GivenUp);
    }
    let counted = webhook_event::Entity::update_many()
        .col_expr(
            webhook_event::Column::Status,
            Expr::value(Status::Processing),
        )
        .col_expr(
            webhook_event::Column::Attempts,
            Expr::col(webhook_event::Column::Attempts).add(1),
        )
        .filter(webhook_event::Column::Id.eq(row.id))
        .exec_with_returning(&txn)
        .await?;
    txn.commit().await?;
    let row = counted
        .into_iter()
        .next()
        .ok_or_else(|| DbErr::RecordNotFound(format!("webhook event {}", row.id)))?;
    Ok(Claimed::Event(row))
}

/// Whether `row` is as [`claim`] left it, so no other instance took it since.
async fn still_claimed(
    txn: &DatabaseTransaction,
    row: &webhook_event::Model,
) -> Result<bool, DbErr> {
    let found = webhook_event::Entity::find_by_id(row.id)
        .filter(webhook_event::Column::Status.eq(Status::Processing))
        .filter(webhook_event::Column::Attempts.eq(row.attempts))
        .one(txn)
        .await?;
    Ok(found.is_some())
}

/// `source`'s first waiting event, in the order LINE sent them.
async fn first_waiting(
    txn: &DatabaseTransaction,
    source: &str,
) -> Result<Option<webhook_event::Model>, DbErr> {
    webhook_event::Entity::find()
        .filter(webhook_event::Column::UserKey.eq(source))
        .filter(webhook_event::Column::Status.is_in(WAITING))
        .order_by_asc(webhook_event::Column::Id)
        .one(txn)
        .await
}

/// Marks event `id` done with, as `status`, now.
async fn finish(
    db: &impl ConnectionTrait,
    id: i64,
    status: Status,
    error: Option<&str>,
) -> Result<(), DbErr> {
    webhook_event::Entity::update_many()
        .col_expr(webhook_event::Column::Status, Expr::value(status))
        .col_expr(webhook_event::Column::ProcessedAt, Expr::value(Utc::now()))
        .col_expr(webhook_event::Column::Error, Expr::value(error))
        .filter(webhook_event::Column::Id.eq(id))
        .exec(db)
        .await?;
    Ok(())
}

/// Fails `row` for good without another attempt.
async fn give_up(txn: &DatabaseTransaction, row: &webhook_event::Model) -> Result<(), DbErr> {
    let error = format!(
        "attempt {} was cut short, and no more are made",
        row.attempts
    );
    finish(txn, row.id, Status::Failed, Some(&error)).await
}

/// Records why attempt `attempt` at event `id` failed, the event becoming `status`: `PENDING`
/// to be tried again, or `FAILED` for good. Changes nothing if the event was taken again since,
/// by another instance.
async fn failed(
    db: &DatabaseConnection,
    id: i64,
    attempt: i32,
    status: Status,
    error: &str,
) -> Result<(), DbErr> {
    let mut update = webhook_event::Entity::update_many()
        .col_expr(webhook_event::Column::Status, Expr::value(status))
        .col_expr(webhook_event::Column::Error, Expr::value(error));
    if status == Status::Failed {
        update = update.col_expr(webhook_event::Column::ProcessedAt, Expr::value(Utc::now()));
    }
    update
        .filter(webhook_event::Column::Id.eq(id))
        .filter(webhook_event::Column::Attempts.eq(attempt))
        .filter(webhook_event::Column::Status.eq(Status::Processing))
        .exec(db)
        .await?;
    Ok(())
}
