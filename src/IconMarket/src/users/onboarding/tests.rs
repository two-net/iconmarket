//! Onboarding (§4.1): the join code's attempts, accepting the notice shown and following
//! again. Each event is handled in a transaction committed once it is handled, as the worker
//! handles them, but at a time of the test's choosing. The `async` tests each start a
//! throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::{ActiveModelTrait, DatabaseConnection, PaginatorTrait, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::line::Body;
use crate::notifications::templates::Templates;
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const CODE: &str = "ICON2569";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

fn minutes(minutes: i64) -> TimeDelta {
    TimeDelta::minutes(minutes)
}

#[test]
fn five_wrong_codes_within_an_hour_block_attempts_for_an_hour() {
    let at = |m: i64| t0() + minutes(m);
    // Newest first, as stored
    let five = [at(40), at(30), at(20), at(10), at(0)];
    assert_eq!(blocked_until(&five, at(40)), Some(at(100)));
    assert_eq!(blocked_until(&five, at(99)), Some(at(100)));
    assert_eq!(blocked_until(&five, at(100)), None);
    // Fewer, or spread over an hour or more, don't block
    assert_eq!(blocked_until(&five[..4], at(40)), None);
    assert_eq!(blocked_until(&[], at(40)), None);
    let spread = [at(60), at(45), at(30), at(15), at(0)];
    assert_eq!(blocked_until(&spread, at(60)), None);
    let within = [at(59), at(45), at(30), at(15), at(0)];
    assert_eq!(blocked_until(&within, at(59)), Some(at(119)));
}

#[test]
fn codes_match_whatever_the_digits_case_or_spaces() {
    for typed in ["ICON2569", " ICON2569 ", "icon2569", "Icon๒๕๖๙"] {
        assert!(is_join_code(typed, CODE), "{typed:?}");
    }
    for typed in ["ICON 2569", "ICON256", "ICON25690", "ไอคอน2569", ""] {
        assert!(!is_join_code(typed, CODE), "{typed:?}");
    }
    assert!(is_join_code("1234", "๑๒๓๔"));
}

/// NFR-PRIV-01, NFR-SEC-03: the button names the version it accepts, as a number.
#[test]
fn fingerprints_follow_the_version_and_travel_in_postbacks() {
    assert_eq!(fingerprint("1"), fingerprint("1"));
    assert_ne!(fingerprint("1"), fingerprint("2"));
    assert_ne!(fingerprint("2026-10"), fingerprint("2026-11"));
    for version in ["1", "2026-10", "ฉบับที่ ๓"] {
        let fingerprint = fingerprint(version);
        assert!(fingerprint >= 0);
        let data = Postback::new(ACCEPT).with_int("v", fingerprint).to_string();
        assert_eq!(Postback::parse(&data).unwrap().int("v"), Some(fingerprint));
    }
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    config: Config,
}

/// The sample config, with a join code.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let mut config = config::sample();
    config.village.join_code = Some(CODE.to_owned());
    Setup {
        _container: container,
        db: orm(pool),
        templates: Templates::new(&config.app),
        config,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `now` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh.
    async fn at(
        &self,
        user_id: i64,
        now: DateTimeUtc,
        handle: impl AsyncFnOnce(&Turn<'_>, &Config) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, now)
            .await
            .unwrap();
        let messages = handle(&turn, &self.config).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        messages
    }

    /// User `user_id` types `text` at `now`.
    async fn typed(&self, user_id: i64, now: DateTimeUtc, text: &str) -> Vec<Message> {
        self.at(user_id, now, async |turn, config| {
            answer(turn, config, Said::Text(text)).await
        })
        .await
    }

    /// User `user_id` taps "ยอมรับ" at `now` on the notice of `version`.
    async fn accepts(&self, user_id: i64, now: DateTimeUtc, version: &str) -> Vec<Message> {
        let mut privacy = config::sample().privacy;
        privacy.notice_version = version.to_owned();
        let postback = accept(&privacy);
        self.at(user_id, now, async |turn, config| {
            tapped(turn, config, &postback).await
        })
        .await
    }

    /// A user who accepted the privacy notice of `version`.
    async fn consented(&self, line_user_id: &str, version: &str) -> i64 {
        let mut user = testing::new_user(line_user_id);
        user.consent_version = Set(Some(version.to_owned()));
        user.consented_at = Set(Some(testing::at()));
        user.insert(&self.db).await.unwrap().id
    }

    async fn user(&self, user_id: i64) -> user::Model {
        user::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn wrong_codes_kept(&self, user_id: i64) -> u64 {
        join_attempt::Entity::find()
            .filter(join_attempt::Column::UserId.eq(user_id))
            .count(&self.db)
            .await
            .unwrap()
    }

    /// The main menu of a user who is only a buyer.
    fn ready(&self) -> Vec<Message> {
        let menu = crate::notifications::templates::main_menu([menu::Tab::Buyer], false);
        vec![self.templates.ready(), menu]
    }
}

/// The alt text of a card, or the text of a text message.
fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        Body::Flex { alt_text, .. } => alt_text,
        body => panic!("unexpected {body:?}"),
    }
}

const ALICE: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BOB: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// AC: the sixth wrong join code within an hour is blocked; so is the right one, until the
/// hour is up.
#[tokio::test]
async fn the_sixth_join_code_within_an_hour_is_blocked() {
    let s = setup().await;
    let t = &s.templates;
    let alice = s.consented(ALICE, "1").await;
    let at = |m: i64| t0() + minutes(m);

    for (i, m) in [0, 10, 20, 30].into_iter().enumerate() {
        let reply = s.typed(alice, at(m), "ICON2568").await;
        assert_eq!(reply, [t.join_code_wrong(4 - i)], "at {m}");
    }
    let blocked = [t.join_code_blocked(at(100))];
    assert_eq!(s.typed(alice, at(40), "icon 2569").await, blocked);
    assert!(text(&blocked[0]).contains("24 ต.ค. 2569 16:10"));
    // Blocked attempts aren't checked or counted, right or wrong
    assert_eq!(s.typed(alice, at(45), CODE).await, blocked);
    assert_eq!(s.typed(alice, at(99), "ICON2568").await, blocked);
    assert_eq!(s.wrong_codes_kept(alice).await, 5);
    assert_eq!(s.user(alice).await.joined_at, None);

    assert_eq!(s.typed(alice, at(100), " icon๒๕๖๙ ").await, s.ready());
    assert_eq!(s.user(alice).await.joined_at, Some(at(100)));
    assert_eq!(s.wrong_codes_kept(alice).await, 0);
}

#[tokio::test]
async fn wrong_codes_count_for_an_hour() {
    let s = setup().await;
    let t = &s.templates;
    let alice = s.consented(ALICE, "1").await;
    let at = |m: i64| t0() + minutes(m);

    for (m, tries_left) in [(0, 4), (15, 3), (30, 2), (45, 1), (60, 1), (76, 1)] {
        assert_eq!(
            s.typed(alice, at(m), "ICON2568").await,
            [t.join_code_wrong(tries_left)],
            "at {m}"
        );
    }
    // Those of more than an hour ago are deleted
    assert_eq!(s.wrong_codes_kept(alice).await, 4);
    assert_eq!(s.typed(alice, at(80), CODE).await, s.ready());
}

/// Anything but text, such as a keyword, a photo or a sticker, is no attempt.
#[tokio::test]
async fn only_text_is_taken_for_a_code() {
    let s = setup().await;
    let alice = s.consented(ALICE, "1").await;
    let village = &s.config.village.name;
    let ask = [s.templates.ask_join_code(village, None)];
    for _ in 0..6 {
        let reply = s
            .at(alice, t0(), async |turn, config| {
                answer(turn, config, Said::Other).await
            })
            .await;
        assert_eq!(reply, ask);
    }
    assert_eq!(s.wrong_codes_kept(alice).await, 0);
    assert_eq!(s.typed(alice, t0(), CODE).await, s.ready());
}

/// FR-ONB-02, NFR-PRIV-01, FR-UI-13: only the notice shown is accepted, once, and the version
/// and time are stored.
#[tokio::test]
async fn only_the_notice_shown_is_accepted() {
    let mut s = setup().await;
    s.config.privacy.notice_version = "2".to_owned();
    let alice = s.consented(ALICE, "1").await;
    let consented_at = s.user(alice).await.consented_at;

    // Until accepted, the new notice is all there is
    let reply = s.typed(alice, t0(), CODE).await;
    assert_eq!(reply.len(), 1);
    assert_eq!(text(&reply[0]), "ประกาศความเป็นส่วนตัวฉบับใหม่");

    // A tap on the old notice shows the new one and accepts nothing
    let reply = s.accepts(alice, t0(), "1").await;
    assert_eq!(text(&reply[0]), "ประกาศความเป็นส่วนตัวฉบับใหม่");
    let user = s.user(alice).await;
    assert_eq!(user.consent_version.as_deref(), Some("1"));
    assert_eq!(user.consented_at, consented_at);

    let ask = [s.templates.ask_join_code(&s.config.village.name, None)];
    assert_eq!(s.accepts(alice, t0(), "2").await, ask);
    let user = s.user(alice).await;
    assert_eq!(user.consent_version.as_deref(), Some("2"));
    assert_eq!(user.consented_at, Some(t0()));

    // A repeated tap changes nothing
    assert_eq!(s.accepts(alice, t0() + minutes(5), "2").await, ask);
    assert_eq!(s.user(alice).await.consented_at, Some(t0()));
}

/// Without a join code, accepting the notice completes onboarding.
#[tokio::test]
async fn without_a_join_code_accepting_is_enough() {
    let mut s = setup().await;
    s.config.village.join_code = None;
    let bob = testing::new_user(BOB).insert(&s.db).await.unwrap().id;

    let reply = s.typed(bob, t0(), "สวัสดี").await;
    assert_eq!(text(&reply[0]), "ประกาศความเป็นส่วนตัว");
    assert_eq!(s.accepts(bob, t0(), "1").await, s.ready());
    let user = s.user(bob).await;
    assert_eq!(
        super::super::pending(&user, &s.config.privacy, &s.config.village),
        None
    );
}

/// FR-ONB-01: following again makes the user a follower once more, and greets them by name.
#[tokio::test]
async fn following_again_greets_the_user_back() {
    let s = setup().await;
    let village = &s.config.village.name;
    let mut alice = testing::new_user(ALICE);
    alice.display_name = Set(Some("ป้าแดง".to_owned()));
    alice.is_following = Set(false);
    alice.consent_version = Set(Some("1".to_owned()));
    alice.joined_at = Set(Some(testing::at()));
    let alice = alice.insert(&s.db).await.unwrap().id;
    let bob = testing::new_user(BOB).insert(&s.db).await.unwrap().id;

    let reply = s.at(alice, t0(), followed).await;
    let menu = crate::notifications::templates::main_menu([menu::Tab::Buyer], false);
    assert_eq!(
        reply,
        [s.templates.welcome_back(Some("ป้าแดง"), village), menu]
    );
    assert!(text(&reply[0]).contains("คุณป้าแดง"));
    let user = s.user(alice).await;
    assert!(user.is_following);
    assert_eq!(user.updated_at, t0());

    let reply = s.at(bob, t0(), followed).await;
    assert_eq!(reply[0], s.templates.welcome(None, village));
    assert!(text(&reply[0]).contains(village));
    assert_eq!(text(&reply[1]), "ประกาศความเป็นส่วนตัว");
}

/// FR-ONB-08
#[tokio::test]
async fn myid_returns_the_senders_own_id() {
    let s = setup().await;
    let bob = testing::new_user(BOB).insert(&s.db).await.unwrap().id;
    let reply = s.at(bob, t0(), async |turn, _| Ok(my_id(turn))).await;
    assert_eq!(reply.len(), 2);
    assert_eq!(text(&reply[1]), BOB);
}
