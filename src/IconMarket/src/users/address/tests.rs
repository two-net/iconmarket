//! The address flow (FR-ONB-04): what each step takes, and that only its last saves. The flow
//! is answered as routing hands it input, each event in a transaction committed once it is
//! handled. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ActiveModelTrait, DatabaseConnection, TransactionTrait};
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

const ALICE: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

fn zones(names: &[&str]) -> Vec<String> {
    names.iter().map(|&name| name.to_owned()).collect()
}

fn draft(ask: Ask) -> AddressFlow {
    AddressFlow {
        ask,
        then: Then::MyData,
        zone: None,
        house_number: None,
        landmark_note: None,
        phone: None,
        location: None,
    }
}

#[test]
fn each_ask_has_its_steps() {
    let steps = |ask: Ask| {
        let mut steps = vec![ask.first_step()];
        while let Some(next) = ask.after(*steps.last().unwrap()) {
            steps.push(next);
        }
        steps
    };
    use Step::*;
    assert_eq!(
        steps(Ask::All),
        [Zone, HouseNumber, Landmark, Phone, Location]
    );
    assert_eq!(steps(Ask::Address), [Zone, HouseNumber, Landmark]);
    assert_eq!(steps(Ask::Phone), [Phone]);
    assert_eq!(steps(Ask::Location), [Location]);
}

/// A zone of `village.zones` matches whatever the spaces, the case or the digits, and is kept
/// as listed.
#[test]
fn zones_match_however_typed() {
    let listed = zones(&["ซอย 3", "หมู่ 5", "Green Ville"]);
    for (typed, as_listed) in [
        ("ซอย 3", "ซอย 3"),
        (" ซอย3 ", "ซอย 3"),
        ("ซอย ๓", "ซอย 3"),
        ("หมู่๕", "หมู่ 5"),
        ("green ville", "Green Ville"),
        ("GREENVILLE", "Green Ville"),
    ] {
        assert_eq!(zone(typed, &listed).as_deref(), Ok(as_listed), "{typed:?}");
    }
    for typed in ["ซอย 4", "ซอย", "3", "หมู่ 5 ซอย 3"] {
        assert_eq!(zone(typed, &listed), Err(Refused::UnknownZone), "{typed:?}");
    }
    assert_eq!(zone("", &listed), Err(Refused::Empty));
}

/// Without `village.zones`, the zone is typed freely, up to its limit.
#[test]
fn zones_are_typed_freely_when_none_are_listed() {
    assert_eq!(zone("หมู่ 5 ซอยวัด", &[]).as_deref(), Ok("หมู่ 5 ซอยวัด"));
    let longest = "ซ".repeat(ZONE_CHARS);
    assert_eq!(zone(&longest, &[]), Ok(longest.clone()));
    let longer = format!("{longest}ซ");
    assert_eq!(zone(&longer, &[]), Err(Refused::TooLong(ZONE_CHARS)));
    assert_eq!(zone("", &[]), Err(Refused::Empty));
}

#[test]
fn each_step_takes_its_answer() {
    let mut all = draft(Ask::All);
    let listed = zones(&["ซอย 3"]);
    let mut take = |step, text: &str| take(step, &mut all, &listed, text);

    assert_eq!(take(Step::Zone, " ซอย๓ "), Ok(()));
    // Thai digits are kept as Arabic, as sellers read them
    assert_eq!(take(Step::HouseNumber, " ๔๕/๒ "), Ok(()));
    assert_eq!(take(Step::Landmark, "รั้วสีเขียว"), Ok(()));
    assert_eq!(take(Step::Phone, "081-234-5678"), Ok(()));
    assert_eq!(
        all,
        AddressFlow {
            zone: Some("ซอย 3".to_owned()),
            house_number: Some("45/2".to_owned()),
            landmark_note: Some("รั้วสีเขียว".to_owned()),
            phone: Some("0812345678".to_owned()),
            ..draft(Ask::All)
        }
    );
}

#[test]
fn optional_answers_can_be_skipped_or_removed() {
    let mut given = AddressFlow {
        landmark_note: Some("ข้างศาลา".to_owned()),
        phone: Some("0812345678".to_owned()),
        location: Some(Point {
            latitude: 13.75,
            longitude: 100.5,
        }),
        ..draft(Ask::All)
    };
    for (step, none) in [
        (Step::Landmark, "ข้าม"),
        (Step::Phone, " ข้าม "),
        (Step::Phone, "ลบเบอร์โทร"),
        (Step::Location, "ข้าม"),
        (Step::Location, "ลบตำแหน่ง"),
    ] {
        assert_eq!(take(step, &mut given, &[], none), Ok(()), "{none:?}");
    }
    assert_eq!(given, draft(Ask::All));
}

#[test]
fn answers_the_steps_dont_take_are_refused() {
    let mut any = draft(Ask::All);
    let mut take = |step, text: &str| take(step, &mut any, &[], text);
    let longest_note = "ข".repeat(LANDMARK_NOTE_CHARS);
    assert_eq!(take(Step::Landmark, &longest_note), Ok(()));
    let longer_note = format!("{longest_note}ข");
    assert_eq!(
        take(Step::Landmark, &longer_note),
        Err(Refused::TooLong(LANDMARK_NOTE_CHARS))
    );
    let longer_number = "1".repeat(HOUSE_NUMBER_CHARS + 1);
    assert_eq!(
        take(Step::HouseNumber, &longer_number),
        Err(Refused::TooLong(HOUSE_NUMBER_CHARS))
    );
    assert_eq!(take(Step::HouseNumber, "   "), Err(Refused::Empty));
    for phone in ["12345", "081234567", "เบอร์บ้าน", "02-123-45678"] {
        assert_eq!(
            take(Step::Phone, phone),
            Err(Refused::NotAPhone),
            "{phone:?}"
        );
    }
    assert_eq!(take(Step::Location, "บ้านสีฟ้า"), Err(Refused::NotALocation));
    // Nothing refused is kept
    assert_eq!(
        any,
        AddressFlow {
            landmark_note: Some(longest_note),
            ..draft(Ask::All)
        }
    );
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    config: Config,
}

/// The sample config, with the village's zones.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let mut config = config::sample();
    config.village.zones = zones(&["ซอย 1", "ซอย 2", "ซอย 3"]);
    Setup {
        _container: container,
        db: orm(pool),
        templates: Templates::new(&config.app),
        config,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `t0()` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh.
    async fn at<T>(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>, &Config) -> Result<T, DbErr>,
    ) -> T {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let result = handle(&turn, &self.config).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        result
    }

    async fn start(&self, user_id: i64, ask: Ask) -> Vec<Message> {
        self.at(user_id, async |turn, config| {
            start(turn, config, ask, Then::MyData).await
        })
        .await
    }

    /// User `user_id` sends `input` to their address flow, as routing hands it over.
    async fn send(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        self.at(user_id, async |turn, config| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<AddressFlow>());
            answer(turn, config, session.expect("in the address flow"), &input).await
        })
        .await
    }

    async fn typed(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.send(user_id, input).await.expect("an answer")
    }

    async fn taps(&self, user_id: i64, postback: &Postback) -> Vec<Message> {
        self.at(user_id, async |turn, config| {
            tapped(turn, config, postback).await
        })
        .await
    }

    /// The step user `user_id`'s address flow waits on, if they are in it.
    async fn step(&self, user_id: i64) -> Option<Step> {
        self.at(user_id, async |turn, _| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<AddressFlow>());
            Ok(session.map(|session| session.step))
        })
        .await
    }

    async fn user(&self, user_id: i64) -> user::Model {
        user::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// "ข้อมูลของฉัน" of user `user_id` as they are now.
    async fn my_data(&self, user_id: i64) -> Vec<Message> {
        self.at(user_id, async |turn, config| {
            profile::show(turn, config, &turn.actor.user).await
        })
        .await
    }

    /// `ask` as a prompt is sent: with `choices` and the cancel button.
    fn prompt(&self, ask: Message, choices: &[Action]) -> Message {
        let ask = if choices.is_empty() {
            ask
        } else {
            ask.with_quick_reply(QuickReply::new(choices.iter().cloned()))
        };
        templates::with_cancel(ask)
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The labels of a message's Quick Replies.
fn choice_labels(message: &Message) -> Vec<&str> {
    let items = &message.quick_reply.as_ref().expect("a Quick Reply").items;
    items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
}

fn says(label: &Label) -> Action {
    Action::message(label.clone(), label.as_str())
}

fn share() -> Action {
    Action::Location {
        label: labels::SHARE_LOCATION.clone(),
    }
}

fn location(latitude: f64, longitude: f64) -> Input {
    Input::Location(Location {
        latitude,
        longitude,
        address: Some("ถนนพหลโยธิน กรุงเทพมหานคร".to_owned()),
    })
}

/// AC (FR-ONB-04): the first time, every part is asked for, the optional ones can be skipped,
/// a wrong answer is explained and asked again, and nothing is saved before the last step.
#[tokio::test]
async fn the_address_is_asked_for_step_by_step_and_saved_at_the_end() {
    let s = setup().await;
    let t = &s.templates;
    let alice = testing::new_user(ALICE).insert(&s.db).await.unwrap().id;
    let zone_choices =
        ["ซอย 1", "ซอย 2", "ซอย 3"].map(|zone| Action::message(Label::fit(zone), zone));
    let skip = [says(&labels::SKIP)];

    let reply = s.start(alice, Ask::All).await;
    assert_eq!(
        reply,
        [s.prompt(t.ask_zone(true, true, None), &zone_choices)]
    );
    assert!(text(&reply[0]).starts_with("ขอที่อยู่สำหรับส่งของก่อนนะคะ"));
    assert_eq!(
        choice_labels(&reply[0]),
        ["ซอย 1", "ซอย 2", "ซอย 3", "ยกเลิก"]
    );

    assert_eq!(
        s.typed(alice, "ซอย 4").await,
        [s.prompt(t.zone_unknown(), &zone_choices)]
    );
    assert_eq!(
        s.typed(alice, "ซอย๓").await,
        [s.prompt(t.ask_house_number(None), &[])]
    );
    assert_eq!(
        s.typed(alice, "๔๕/๒").await,
        [s.prompt(t.ask_landmark(100, None), &skip)]
    );
    let note = "ข".repeat(101);
    assert_eq!(
        s.typed(alice, &note).await,
        [s.prompt(t.text_too_long(100), &skip)]
    );
    assert_eq!(
        s.typed(alice, "ข้าม").await,
        [s.prompt(t.ask_phone(None), &skip)]
    );

    // AC: an invalid Thai phone number is refused with the formats taken; the step waits
    let refused = s.typed(alice, "12345").await;
    assert_eq!(refused, [s.prompt(t.phone_invalid(false), &skip)]);
    assert!(text(&refused[0]).contains("0812345678") && text(&refused[0]).contains("9 หลัก"));
    assert_eq!(s.step(alice).await, Some(Step::Phone));

    let ask_location = [s.prompt(t.ask_location(false), &[share(), says(&labels::SKIP)])];
    assert_eq!(s.typed(alice, "081-234-5678").await, ask_location);
    assert_eq!(
        choice_labels(&ask_location[0]),
        ["แชร์ตำแหน่ง", "ข้าม", "ยกเลิก"]
    );
    // A photo is no answer here; routing goes on (FR-UI-14)
    let photo = Input::Image {
        message_id: "1".to_owned(),
    };
    assert_eq!(s.send(alice, photo).await, None);
    assert_eq!(
        s.typed(alice, "บ้านสีฟ้า").await,
        [s.prompt(t.location_expected(false), &[share(), says(&labels::SKIP)])]
    );
    // Nothing is saved yet
    let before = s.user(alice).await;
    assert_eq!((before.zone, before.phone), (None, None));

    let reply = s.send(alice, location(13.75, 100.5)).await.unwrap();
    assert_eq!(reply[0], t.address_saved("ซอย 3 บ้านเลขที่ 45/2"));
    assert_eq!(reply[1..], s.my_data(alice).await);
    assert_eq!(s.step(alice).await, None, "the flow is over");
    let user = s.user(alice).await;
    assert_eq!(user.zone.as_deref(), Some("ซอย 3"));
    assert_eq!(user.house_number.as_deref(), Some("45/2"));
    assert_eq!(user.landmark_note, None);
    assert_eq!(user.phone.as_deref(), Some("0812345678"));
    assert_eq!((user.latitude, user.longitude), (Some(13.75), Some(100.5)));
    assert_eq!(user.updated_at, t0());
    assert!(has_address(&user));
}

/// FR-UI-04: cancelling discards the answers so far.
#[tokio::test]
async fn cancelling_saves_nothing() {
    let s = setup().await;
    let alice = testing::new_user(ALICE).insert(&s.db).await.unwrap().id;
    s.start(alice, Ask::All).await;
    s.typed(alice, "ซอย 1").await;
    s.typed(alice, "12").await;
    s.at(alice, async |turn, _| flow::cancel(turn).await).await;

    assert_eq!(s.step(alice).await, None);
    let user = s.user(alice).await;
    assert_eq!((user.zone, user.house_number), (None, None));
    assert!(!has_address(&s.user(alice).await));
}

/// FR-ONB-05, NFR-PRIV-04: correcting one part shows what it is now, changes only that part,
/// and can remove the optional ones.
#[tokio::test]
async fn correcting_a_part_changes_only_that_part() {
    let s = setup().await;
    let t = &s.templates;
    let mut alice = testing::new_user(ALICE);
    alice.zone = Set(Some("ซอย 1".to_owned()));
    alice.house_number = Set(Some("9".to_owned()));
    alice.landmark_note = Set(Some("ข้างศาลา".to_owned()));
    alice.phone = Set(Some("0812345678".to_owned()));
    alice.latitude = Set(Some(13.75));
    alice.longitude = Set(Some(100.5));
    let alice = alice.insert(&s.db).await.unwrap();
    let id = alice.id;

    let remove_phone = [says(&labels::REMOVE_PHONE)];
    let reply = s.start(id, Ask::Phone).await;
    assert_eq!(
        reply,
        [s.prompt(t.ask_phone(Some("081-234-5678")), &remove_phone)]
    );
    assert!(text(&reply[0]).contains("(ตอนนี้: 081-234-5678)"));
    assert_eq!(
        s.typed(id, "0812").await,
        [s.prompt(t.phone_invalid(true), &remove_phone)]
    );
    let reply = s.typed(id, "02 123 4567").await;
    assert_eq!(reply[0], t.phone_saved(Some("02-123-4567")));
    assert_eq!(reply[1..], s.my_data(id).await);
    let user = s.user(id).await;
    assert_eq!(user.phone.as_deref(), Some("021234567"));
    assert_eq!(
        user::Model {
            phone: alice.phone.clone(),
            updated_at: alice.updated_at,
            ..user
        },
        alice,
        "nothing else changed"
    );

    s.start(id, Ask::Phone).await;
    let reply = s.typed(id, "ลบเบอร์โทร").await;
    assert_eq!(reply[0], t.phone_saved(None));
    assert_eq!(s.user(id).await.phone, None);
    // Without a phone there is nothing to remove, only to skip
    assert_eq!(
        s.start(id, Ask::Phone).await,
        [s.prompt(t.ask_phone(None), &[says(&labels::SKIP)])]
    );

    let reply = s.start(id, Ask::Location).await;
    let remove_location = [share(), says(&labels::REMOVE_LOCATION)];
    assert_eq!(reply, [s.prompt(t.ask_location(true), &remove_location)]);
    s.send(id, location(13.8, 100.55)).await.unwrap();
    let user = s.user(id).await;
    assert_eq!((user.latitude, user.longitude), (Some(13.8), Some(100.55)));
    s.start(id, Ask::Location).await;
    assert_eq!(s.typed(id, "ลบตำแหน่ง").await[0], t.location_saved(false));
    let user = s.user(id).await;
    assert_eq!((user.latitude, user.longitude), (None, None));

    let reply = s.start(id, Ask::Address).await;
    assert!(!text(&reply[0]).starts_with("ขอที่อยู่"), "not the first time");
    assert!(text(&reply[0]).ends_with("(ตอนนี้: ซอย 1)"));
    let reply = s.typed(id, "ซอย 2").await;
    assert!(text(&reply[0]).ends_with("(ตอนนี้: 9)"));
    s.typed(id, "10/1").await;
    let reply = s.typed(id, "ข้าม").await;
    assert_eq!(reply[0], t.address_saved("ซอย 2 บ้านเลขที่ 10/1"));
    let user = s.user(id).await;
    assert_eq!(
        (user.zone, user.house_number, user.landmark_note),
        (Some("ซอย 2".to_owned()), Some("10/1".to_owned()), None)
    );
    assert_eq!(user.phone, None, "untouched");
}

/// FR-LINE-11: more zones than Quick Replies hold are paged with "ดูเพิ่มเติม".
#[tokio::test]
async fn long_lists_of_zones_are_paged() {
    let mut s = setup().await;
    s.config.village.zones = (1..=15).map(|i| format!("ซอย {i}")).collect();
    let alice = testing::new_user(ALICE).insert(&s.db).await.unwrap().id;
    let more = |page| Postback::new(MORE_ZONES).with_int(PAGE, page);

    let first = s.start(alice, Ask::All).await;
    let labels = choice_labels(&first[0]);
    assert_eq!(labels.len(), limits::QUICK_REPLY_ITEMS);
    assert_eq!(labels[..2], ["ซอย 1", "ซอย 2"]);
    assert_eq!(labels[11..], ["ดูเพิ่มเติม", "ยกเลิก"]);
    limits::check(&first).unwrap();

    let second = s.taps(alice, &more(1)).await;
    assert_eq!(
        choice_labels(&second[0]),
        ["ซอย 12", "ซอย 13", "ซอย 14", "ซอย 15", "ยกเลิก"]
    );
    // A page past the last shows the first
    assert_eq!(s.taps(alice, &more(9)).await, first);
    // Any zone can be typed, wherever its page
    s.typed(alice, "ซอย 15").await;
    assert_eq!(s.step(alice).await, Some(Step::HouseNumber));
    // Once the zone is answered, its pages are gone
    let gone = [s.templates.denied(Denied::NotFound)];
    assert_eq!(s.taps(alice, &more(1)).await, gone);
    assert_eq!(s.step(alice).await, Some(Step::HouseNumber));
    assert_eq!(s.taps(alice, &Postback::new("address.other")).await, gone);
}

/// Without `village.zones`, the zone is typed.
#[tokio::test]
async fn without_listed_zones_the_zone_is_typed() {
    let mut s = setup().await;
    s.config.village.zones = Vec::new();
    let alice = testing::new_user(ALICE).insert(&s.db).await.unwrap().id;

    let reply = s.start(alice, Ask::Address).await;
    assert_eq!(
        reply,
        [s.prompt(s.templates.ask_zone(false, false, None), &[])]
    );
    assert!(text(&reply[0]).contains("พิมพ์มาได้เลย"));
    s.typed(alice, "หมู่ 5 ซอยวัด").await;
    s.typed(alice, "45/2").await;
    s.typed(alice, "ตรงข้ามศาลา").await;
    let user = s.user(alice).await;
    assert_eq!(user.zone.as_deref(), Some("หมู่ 5 ซอยวัด"));
    assert_eq!(user.landmark_note.as_deref(), Some("ตรงข้ามศาลา"));
    assert_eq!(user.phone, None, "only the address was asked for");
}
