//! The conversation framework (#13) against a database: guided flows, cancelling and
//! confirmations. Each test starts a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use chrono_tz::Asia::Bangkok;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, IntoActiveModel,
    QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;
use url::Url;

use super::flow::{self, Answer, Flow, Session};
use super::pending::{self, Applied, CONFIRM, Proposal, Stale, confirm, propose};
use super::postback::Postback;
use super::*;
use crate::config::{AppConfig, PoliteParticle};
use crate::line::{Action, Body};
use crate::notifications::templates::labels;
use crate::persistence::entity::{counter, user};
use crate::persistence::{orm, random_id, testing};
use crate::users::access::{self, Denied};

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

async fn database() -> (ContainerAsync<Postgres>, DatabaseConnection) {
    let (container, pool) = testing::migrated().await;
    (container, orm(pool))
}

fn templates() -> Templates {
    Templates::new(&AppConfig {
        name: "IconMarket".to_owned(),
        base_url: Url::parse("https://market.example.com").unwrap(),
        listen_port: 8080,
        time_zone: Bangkok,
        polite_particle: PoliteParticle::Kha,
    })
}

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

fn minutes(minutes: i64) -> TimeDelta {
    TimeDelta::minutes(minutes)
}

async fn new_user(db: &DatabaseConnection, line_user_id: &str) -> i64 {
    testing::new_user(line_user_id).insert(db).await.unwrap().id
}

/// User `user_id`'s event at `now`, with the user loaded afresh as for each event.
async fn turn<'a>(
    db: &'a DatabaseConnection,
    templates: &'a Templates,
    user_id: i64,
    now: DateTimeUtc,
) -> Turn<'a> {
    let actor = Actor::load(db, user_id).await.unwrap().expect("a user");
    Turn::new(db, templates, &SESSION, actor, now)
        .await
        .unwrap()
}

fn texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| match &message.body {
            Body::Text { text } => text.as_str(),
            Body::Flex { alt_text, .. } => alt_text.as_str(),
            body => panic!("unexpected {body:?}"),
        })
        .collect()
}

// Confirmations

/// Counts the bumps applied, as a proposal's effect.
const BUMPS: &str = "test:bumps";

async fn bumps(db: &impl ConnectionTrait) -> i64 {
    counter::Entity::find_by_id(BUMPS)
        .one(db)
        .await
        .unwrap()
        .map_or(0, |counter| counter.value)
}

async fn start_counting(db: &DatabaseConnection) {
    let counter = counter::ActiveModel {
        key: Set(BUMPS.to_owned()),
        value: Set(0),
    };
    counter.insert(db).await.unwrap();
}

/// Adds a bump outside any proposal, as when a price changes after a summary was shown.
async fn bump(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    counter::Entity::update_many()
        .col_expr(
            counter::Column::Value,
            Expr::col(counter::Column::Value).add(1),
        )
        .filter(counter::Column::Key.eq(BUMPS))
        .exec(db)
        .await?;
    Ok(())
}

/// A proposal to add one bump, applying only while the count is still `seen`, as a checkout
/// applies only at the prices shown.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Bump {
    seen: i64,
    admin_only: bool,
}

impl Proposal for Bump {
    const KIND: &'static str = "test.bump";

    fn requires(&self) -> access::Action {
        if self.admin_only {
            access::Action::Administer
        } else {
            access::Action::Buy
        }
    }

    async fn apply(
        &self,
        _id: Uuid,
        txn: &DatabaseTransaction,
        _turn: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        // Changes first and checks after, to show that a change that doesn't apply is undone
        bump(txn).await?;
        let now = bumps(txn).await;
        if now != self.seen + 1 {
            return Ok(Applied::Changed);
        }
        let done = Message::text(format!("bumped to {now}"));
        Ok(Applied::Done(vec![done]))
    }

    async fn current_state(stale: Stale<Self>, turn: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        let (why, stored) = (stale.why, stale.proposal.is_some());
        let state = format!("{why:?} at {}, stored {stored}", bumps(turn.db).await);
        Ok(vec![Message::text(state)])
    }
}

/// Another kind, whose confirm can't be used for a bump.
#[derive(Debug, Serialize, Deserialize)]
struct Other;

impl Proposal for Other {
    const KIND: &'static str = "test.other";

    fn requires(&self) -> access::Action {
        access::Action::Help
    }

    async fn apply(
        &self,
        _: Uuid,
        _: &DatabaseTransaction,
        _: &Turn<'_>,
    ) -> Result<Applied, DbErr> {
        Ok(Applied::Done(vec![Message::text("other done")]))
    }

    async fn current_state(stale: Stale<Self>, _: &Turn<'_>) -> Result<Vec<Message>, DbErr> {
        let (why, stored) = (stale.why, stale.proposal.is_some());
        Ok(vec![Message::text(format!(
            "other {why:?}, stored {stored}"
        ))])
    }
}

/// Proposes `proposal` as user `user_id` at t0, and returns the ID its button carries, read
/// back as LINE sends it.
async fn proposed<P: Proposal>(
    db: &DatabaseConnection,
    templates: &Templates,
    user_id: i64,
    proposal: &P,
) -> Uuid {
    let turn = turn(db, templates, user_id, t0()).await;
    let postback = propose(&turn, proposal).await.unwrap();
    let postback = Postback::parse(&postback.to_string()).unwrap();
    assert_eq!(postback.action(), CONFIRM);
    assert_eq!(postback.name("k"), Some(P::KIND));
    postback.uuid("pa").unwrap()
}

/// User `user_id` taps the confirm button of pending action `id` at `now`.
async fn tap<P: Proposal>(
    db: &DatabaseConnection,
    templates: &Templates,
    user_id: i64,
    id: Uuid,
    now: DateTimeUtc,
) -> Vec<String> {
    let turn = turn(db, templates, user_id, now).await;
    let reply = confirm::<P>(&turn, id).await.unwrap();
    texts(&reply).into_iter().map(str::to_owned).collect()
}

async fn consumed(db: &DatabaseConnection, id: Uuid) -> Option<bool> {
    let row = pending_action::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap();
    row.map(|row| row.consumed_at.is_some())
}

/// AC (#13): tapping a confirmation twice applies it once; the second tap shows the current
/// state.
#[tokio::test]
async fn tapping_a_confirmation_twice_applies_it_once() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let buyer = new_user(&db, "U-buyer").await;
    let id = proposed(&db, &t, buyer, &Bump::default()).await;
    assert_eq!(consumed(&db, id).await, Some(false));

    let first = tap::<Bump>(&db, &t, buyer, id, t0() + minutes(1)).await;
    assert_eq!(first, ["bumped to 1"]);
    let second = tap::<Bump>(&db, &t, buyer, id, t0() + minutes(2)).await;
    assert_eq!(second, ["Confirmed at 1, stored true"]);
    assert_eq!(bumps(&db).await, 1);
    assert_eq!(consumed(&db, id).await, Some(true));
}

/// FR-UI-13, NFR-REL-02: taps arriving together apply once.
#[tokio::test]
async fn taps_arriving_together_apply_once() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let buyer = new_user(&db, "U-buyer").await;
    let id = proposed(&db, &t, buyer, &Bump::default()).await;

    let now = t0() + minutes(1);
    let (one, other) = (
        turn(&db, &t, buyer, now).await,
        turn(&db, &t, buyer, now).await,
    );
    let (one, other) = tokio::join!(confirm::<Bump>(&one, id), confirm::<Bump>(&other, id));
    let mut replies = [one.unwrap(), other.unwrap()].map(|reply| texts(&reply).join(""));
    replies.sort();
    assert_eq!(replies, ["Confirmed at 1, stored true", "bumped to 1"]);
    assert_eq!(bumps(&db).await, 1);
}

/// AC (#13): a confirmation tapped after it expired, whether or not it was deleted yet, shows
/// the current state and does nothing else.
#[tokio::test]
async fn an_expired_confirmation_shows_the_current_state_and_does_nothing_else() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let buyer = new_user(&db, "U-buyer").await;
    let id = proposed(&db, &t, buyer, &Bump::default()).await;
    let expiry = t0() + minutes(SESSION.pending_action_minutes.into());

    let at_expiry = tap::<Bump>(&db, &t, buyer, id, expiry).await;
    assert_eq!(at_expiry, ["Gone at 0, stored true"]);
    assert_eq!(consumed(&db, id).await, Some(false));

    let later = expiry + minutes(1);
    delete_expired(&db, later).await.unwrap();
    let deleted = tap::<Bump>(&db, &t, buyer, id, later).await;
    assert_eq!(deleted, ["Gone at 0, stored false"]);
    assert_eq!(bumps(&db).await, 0);
}

/// FR-UI-13: when what a proposal acts on changed, nothing applies and the current state is
/// shown; the proposal can be tapped again.
#[tokio::test]
async fn a_changed_proposal_applies_nothing() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let buyer = new_user(&db, "U-buyer").await;
    let id = proposed(&db, &t, buyer, &Bump::default()).await;
    bump(&db).await.unwrap();

    let reply = tap::<Bump>(&db, &t, buyer, id, t0() + minutes(1)).await;
    assert_eq!(reply, ["Changed at 1, stored true"]);
    assert_eq!(bumps(&db).await, 1, "the bump it made is undone");
    assert_eq!(consumed(&db, id).await, Some(false));

    // A summary proposed again at the new state applies
    let again = Bump {
        seen: 1,
        ..Bump::default()
    };
    let id = proposed(&db, &t, buyer, &again).await;
    let reply = tap::<Bump>(&db, &t, buyer, id, t0() + minutes(2)).await;
    assert_eq!(reply, ["bumped to 2"]);
}

/// NFR-SEC-02, FR-UI-13: the user's rights are checked at the tap, as they are then.
#[tokio::test]
async fn a_tap_applies_only_with_the_users_rights_at_the_time() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let user_id = new_user(&db, "U-buyer").await;
    let admin_only = Bump {
        admin_only: true,
        ..Bump::default()
    };
    let id = proposed(&db, &t, user_id, &admin_only).await;

    let set = |is_admin, status| {
        let db = &db;
        async move {
            let mut user = user::Entity::find_by_id(user_id)
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .into_active_model();
            user.is_admin = Set(is_admin);
            user.status = Set(status);
            user.update(db).await.unwrap();
        }
    };

    let refused = tap::<Bump>(&db, &t, user_id, id, t0() + minutes(1)).await;
    assert_eq!(refused, texts(&[t.denied(Denied::NotAllowed)]));
    assert_eq!(consumed(&db, id).await, Some(false));

    set(true, user::Status::Suspended).await;
    let suspended = tap::<Bump>(&db, &t, user_id, id, t0() + minutes(2)).await;
    assert_eq!(suspended, texts(&[t.denied(Denied::Suspended)]));
    assert_eq!(bumps(&db).await, 0);

    set(true, user::Status::Active).await;
    let applied = tap::<Bump>(&db, &t, user_id, id, t0() + minutes(3)).await;
    assert_eq!(applied, ["bumped to 1"]);
}

/// A pending action is confirmed only by its user, and only as its kind.
#[tokio::test]
async fn a_confirmation_is_only_its_users_and_its_kinds() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let buyer = new_user(&db, "U-buyer").await;
    let other_buyer = new_user(&db, "U-other").await;
    let id = proposed(&db, &t, buyer, &Bump::default()).await;
    let now = t0() + minutes(1);

    let theirs = tap::<Bump>(&db, &t, other_buyer, id, now).await;
    assert_eq!(theirs, ["Gone at 0, stored false"]);
    let as_other = tap::<Other>(&db, &t, buyer, id, now).await;
    assert_eq!(as_other, ["other Gone, stored false"]);
    let forged = tap::<Bump>(&db, &t, buyer, random_id(), now).await;
    assert_eq!(forged, ["Gone at 0, stored false"]);
    assert_eq!(consumed(&db, id).await, Some(false));

    let own = tap::<Bump>(&db, &t, buyer, id, now).await;
    assert_eq!(own, ["bumped to 1"]);
}

/// A confirm button of no kind this release knows finds nothing.
#[tokio::test]
async fn a_tap_on_an_unknown_kind_finds_nothing() {
    let (_container, db) = database().await;
    let t = templates();
    let buyer = new_user(&db, "U-buyer").await;
    let turn = turn(&db, &t, buyer, t0()).await;
    for data in [
        format!("confirm?k=test.gone&pa={}", random_id()),
        "confirm?k=test.bump".to_owned(),
        "confirm".to_owned(),
    ] {
        let postback = Postback::parse(&data).unwrap();
        let reply = pending::tapped(&turn, &postback).await.unwrap();
        assert_eq!(reply, [t.denied(Denied::NotFound)], "{data}");
    }
}

// Guided flows

/// A two-step flow, as a listing's
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Listing {
    name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    Name,
    Price,
}

impl Flow for Listing {
    const NAME: &'static str = "test.listing";
    type Step = Step;
}

fn ask(text: &str) -> Vec<Message> {
    vec![Message::text(text)]
}

fn cancel_button() -> Action {
    Action::message(labels::CANCEL.clone(), "ยกเลิก")
}

/// The Quick Reply buttons of the last message.
fn quick_replies(messages: &[Message]) -> Vec<Action> {
    let last = messages.last().expect("a message");
    let items = last.quick_reply.as_ref().map(|quick| &quick.items);
    items
        .into_iter()
        .flatten()
        .map(|item| item.action.clone())
        .collect()
}

async fn stored(db: &DatabaseConnection, user_id: i64) -> Option<session::Model> {
    session::Entity::find_by_id(user_id).one(db).await.unwrap()
}

/// Starts the listing flow for `user_id` at t0.
async fn start_listing(db: &DatabaseConnection, templates: &Templates, user_id: i64) {
    let turn = turn(db, templates, user_id, t0()).await;
    let session = Session {
        step: Step::Name,
        draft: Listing::default(),
    };
    let reply = flow::start(&turn, session, ask("ชื่อสินค้า")).await.unwrap();
    assert_eq!(quick_replies(&reply), [cancel_button()]);
}

/// AC (#13): a flow left unanswered for `session.timeoutMinutes` ends, and the reply to the
/// user's next event says so, once. Each answer starts the timeout again.
#[tokio::test]
async fn an_idle_flow_says_it_expired_on_the_next_message() {
    let (_container, db) = database().await;
    let t = templates();
    let seller = new_user(&db, "U-seller").await;
    start_listing(&db, &t, seller).await;
    let timeout = minutes(SESSION.timeout_minutes.into());
    let second = TimeDelta::seconds(1);

    // An answer the step doesn't take, just before the timeout
    let answered_at = t0() + timeout - second;
    let answering = turn(&db, &t, seller, answered_at).await;
    let flow = answering.flow.as_ref().expect("still in progress");
    assert_eq!(flow.flow(), Listing::NAME);
    assert_eq!(flow.read::<Listing>().unwrap().step, Step::Name);
    let again = Answer::<Listing>::Again(ask("ชื่อยาวเกินไป"));
    let reply = flow::answered(&answering, again).await.unwrap();
    assert_eq!(quick_replies(&reply), [cancel_button()]);
    assert_eq!(
        answering.reply(reply.clone()),
        reply,
        "no notice while active"
    );

    // Still in progress after the first timeout, as it was answered
    let still = turn(&db, &t, seller, t0() + timeout + second).await;
    assert!(still.flow.is_some());

    let expiry = answered_at + timeout;
    let next = turn(&db, &t, seller, expiry).await;
    assert!(next.flow.is_none());
    assert_eq!(stored(&db, seller).await, None);
    let menu = menu::main_menu(&next.actor);
    let reply = next.reply(vec![menu.clone()]);
    assert_eq!(reply, [t.step_expired(15), menu.clone()]);
    assert!(texts(&reply)[0].contains("หมดเวลา"));

    // Told once
    let after = turn(&db, &t, seller, expiry + second).await;
    assert_eq!(after.reply(vec![menu.clone()]), [menu]);
}

#[tokio::test]
async fn answers_move_a_flow_on_until_it_is_done() {
    let (_container, db) = database().await;
    let t = templates();
    let seller = new_user(&db, "U-seller").await;
    start_listing(&db, &t, seller).await;

    let turn_at = |minute| turn(&db, &t, seller, t0() + minutes(minute));
    let named = turn_at(1).await;
    let mut session = named.flow.as_ref().unwrap().read::<Listing>().unwrap();
    session.step = Step::Price;
    session.draft.name = Some("ไข่ไก่".to_owned());
    let reply = flow::answered(&named, Answer::Next(session.clone(), ask("ราคาเท่าไร")))
        .await
        .unwrap();
    assert_eq!(quick_replies(&reply), [cancel_button()]);
    let row = stored(&db, seller).await.unwrap();
    assert_eq!(
        (row.step.as_str(), row.data),
        ("price", json!({ "name": "ไข่ไก่" }))
    );
    assert_eq!(row.expires_at, t0() + minutes(1 + 15));

    let priced = turn_at(2).await;
    assert_eq!(
        priced.flow.as_ref().unwrap().read::<Listing>(),
        Some(session)
    );
    let done = Answer::<Listing>::Done(ask("ลงขายแล้ว"));
    let reply = flow::answered(&priced, done).await.unwrap();
    assert_eq!(texts(&reply), ["ลงขายแล้ว"]);
    assert_eq!(stored(&db, seller).await, None);
}

/// A flow this release can't continue ends as if it had expired.
#[tokio::test]
async fn a_flow_that_cant_be_read_ends_as_expired() {
    let (_container, db) = database().await;
    let t = templates();
    let seller = new_user(&db, "U-seller").await;
    // Left by an older release, with a step since removed
    let row = session::ActiveModel {
        user_id: Set(seller),
        flow: Set(Listing::NAME.to_owned()),
        step: Set("photos".to_owned()),
        data: Set(json!({})),
        expires_at: Set(t0() + minutes(15)),
    };
    row.insert(&db).await.unwrap();

    let mut turn = turn(&db, &t, seller, t0()).await;
    assert_eq!(turn.flow.as_ref().unwrap().read::<Listing>(), None);
    turn.expire_flow().await.unwrap();
    assert!(turn.flow.is_none());
    assert_eq!(stored(&db, seller).await, None);
    assert_eq!(turn.reply(Vec::new()), [t.step_expired(15)]);
}

/// FR-UI-04: `ยกเลิก` discards the flow's draft and the changes awaiting confirmation, and shows
/// the main menu.
#[tokio::test]
async fn cancelling_discards_the_draft_and_shows_the_main_menu() {
    let (_container, db) = database().await;
    let t = templates();
    start_counting(&db).await;
    let buyer = new_user(&db, "U-buyer").await;
    let confirmed = proposed(&db, &t, buyer, &Bump::default()).await;
    tap::<Bump>(&db, &t, buyer, confirmed, t0() + minutes(1)).await;
    let open = proposed(&db, &t, buyer, &Bump::default()).await;
    start_listing(&db, &t, buyer).await;

    assert!(flow::is_cancel(" ยกเลิก "));
    assert!(!flow::is_cancel("ยกเลิกออเดอร์"));
    let cancelling = turn(&db, &t, buyer, t0() + minutes(2)).await;
    let reply = flow::cancel(&cancelling).await.unwrap();
    let menu = menu::main_menu(&cancelling.actor);
    assert_eq!(reply, [t.cancelled(), menu.clone()]);
    assert_eq!(stored(&db, buyer).await, None);
    assert_eq!(consumed(&db, open).await, None, "discarded");
    assert_eq!(
        consumed(&db, confirmed).await,
        Some(true),
        "kept until it expires"
    );

    // The cancelled card's button changes nothing
    let late = tap::<Bump>(&db, &t, buyer, open, t0() + minutes(3)).await;
    assert_eq!(late, ["Gone at 1, stored false"]);
    let again = tap::<Bump>(&db, &t, buyer, confirmed, t0() + minutes(3)).await;
    assert_eq!(again, ["Confirmed at 1, stored true"]);

    // Nothing left to cancel: just the menu
    let idle = turn(&db, &t, buyer, t0() + minutes(4)).await;
    assert_eq!(flow::cancel(&idle).await.unwrap(), [menu]);
}

// Clean-up

async fn new_session(db: &DatabaseConnection, user_id: i64, expires_at: DateTimeUtc) {
    let session = session::ActiveModel {
        user_id: Set(user_id),
        flow: Set("add_product".to_owned()),
        step: Set("price".to_owned()),
        data: Set(json!({ "name": "ไข่ไก่" })),
        expires_at: Set(expires_at),
    };
    session.insert(db).await.unwrap();
}

async fn new_pending_action(
    db: &DatabaseConnection,
    user_id: i64,
    expires_at: DateTimeUtc,
    consumed: bool,
) -> Uuid {
    let action = pending_action::ActiveModel {
        id: Set(random_id()),
        user_id: Set(user_id),
        kind: Set("checkout".to_owned()),
        payload: Set(json!({ "shop_id": 1 })),
        expires_at: Set(expires_at),
        consumed_at: Set(consumed.then_some(expires_at - TimeDelta::minutes(30))),
        created_at: Set(expires_at - TimeDelta::hours(1)),
    };
    action.insert(db).await.unwrap().id
}

async fn sessions(db: &DatabaseConnection) -> Vec<i64> {
    session::Entity::find()
        .select_only()
        .column(session::Column::UserId)
        .order_by_asc(session::Column::UserId)
        .into_tuple()
        .all(db)
        .await
        .unwrap()
}

async fn pending_actions(db: &DatabaseConnection) -> Vec<Uuid> {
    pending_action::Entity::find()
        .select_only()
        .column(pending_action::Column::Id)
        .into_tuple()
        .all(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn expired_sessions_and_pending_actions_are_deleted() {
    let (_container, db) = database().await;
    let now = Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap();
    let second = TimeDelta::seconds(1);

    let [gone, expired_today, active] = [
        "U0123456789abcdef0123456789abcde0",
        "U0123456789abcdef0123456789abcde1",
        "U0123456789abcdef0123456789abcde2",
    ];
    let (gone, expired_today, active) = (
        new_user(&db, gone).await,
        new_user(&db, expired_today).await,
        new_user(&db, active).await,
    );
    new_session(&db, gone, now - EXPIRED_SESSION_GRACE - second).await;
    new_session(&db, expired_today, now - EXPIRED_SESSION_GRACE + second).await;
    new_session(&db, active, now + TimeDelta::minutes(15)).await;
    new_pending_action(&db, active, now - second, false).await;
    new_pending_action(&db, active, now - second, true).await;
    let open = new_pending_action(&db, active, now + second, false).await;

    let deleted = delete_expired(&db, now).await.unwrap();
    assert_eq!(
        deleted,
        Deleted {
            sessions: 1,
            pending_actions: 2
        }
    );
    assert_eq!(sessions(&db).await, [expired_today, active]);
    assert_eq!(pending_actions(&db).await, [open]);

    // Running it again has no further effect
    let again = delete_expired(&db, now).await.unwrap();
    assert_eq!(
        again,
        Deleted {
            sessions: 0,
            pending_actions: 0
        }
    );
    assert_eq!(sessions(&db).await, [expired_today, active]);
    assert_eq!(pending_actions(&db).await, [open]);
}
