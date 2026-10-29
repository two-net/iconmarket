//! Rating an order (FR-ORD-14): once an order is completed, its buyer taps "ให้คะแนน", picks 1 to
//! 5 stars, which are kept at once, then types what to tell the shop or skips it. An order is
//! rated once, by its buyer only; its cards show the rating to both parties, and a shop's card
//! its average from the third rating (FR-SHOP-09). Each event is handled in a transaction
//! committed once it is handled, with what it notifies queued in it. The tests each start a
//! throwaway PostgreSQL, so they need Docker.

use std::cell::Cell;
use std::slice;

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{ActiveModelTrait, DatabaseConnection, PaginatorTrait, QueryOrder, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, Config, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::flex::{Bubble, Component, FlexContainer};
use crate::line::{Body, limits};
use crate::notifications::templates::{Templates, order_status_name};
use crate::orders::{COMPLETE, RECEIVED, VIEW, lists};
use crate::persistence::entity::order::{Fulfillment, Status};
use crate::persistence::entity::{push_log, session, user};
use crate::persistence::{orm, random_id, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const SELLER: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const BUYER: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";
const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const OTHER_SELLER: &str = "Ue5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5";
const OTHER_BUYER: &str = "Uf6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6";

/// 25 ต.ค. 2569 09:00 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 25, 2, 0, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
    seller: i64,
    buyer: i64,
    admin: i64,
    other_seller: i64,
    other_buyer: i64,
    shop: shop::Model,
    other_shop: shop::Model,
    orders: Cell<u32>,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let seller = testing::new_user(SELLER).insert(&db).await.unwrap().id;
    let buyer = user::ActiveModel {
        display_name: Set(Some("สมชาย".to_owned())),
        ..testing::new_user(BUYER)
    };
    let buyer = buyer.insert(&db).await.unwrap().id;
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(ADMIN)
    };
    let admin = admin.insert(&db).await.unwrap().id;
    let other_seller = testing::new_user(OTHER_SELLER)
        .insert(&db)
        .await
        .unwrap()
        .id;
    let other_shop = testing::new_shop(other_seller).insert(&db).await.unwrap();
    let other_buyer = testing::new_user(OTHER_BUYER).insert(&db).await.unwrap().id;
    let shop = shop::ActiveModel {
        name: Set("ร้านป้าแดง".to_owned()),
        ..testing::new_shop(seller)
    };
    let shop = shop.insert(&db).await.unwrap();
    let config = config::sample();
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config.app),
        config,
        seller,
        buyer,
        admin,
        other_seller,
        other_buyer,
        shop,
        other_shop,
        orders: Cell::new(0),
    }
}

impl Setup {
    /// An order of `buyer`'s at shop `shop_id`, `status`, picked up and paid in cash. The first
    /// is IM-20261025-0001.
    async fn order_at(&self, buyer: i64, shop_id: i64, status: Status) -> order::Model {
        self.orders.set(self.orders.get() + 1);
        let number = format!("IM-20261025-{:04}", self.orders.get());
        order::ActiveModel {
            status: Set(status),
            fulfillment: Set(Fulfillment::Pickup),
            ..testing::new_order(buyer, shop_id, &number, random_id())
        }
        .insert(&self.db)
        .await
        .unwrap()
    }

    /// An order of the buyer's at the shop, `status`.
    async fn order(&self, status: Status) -> order::Model {
        self.order_at(self.buyer, self.shop.id, status).await
    }

    /// An order of the buyer's at the shop, completed.
    async fn completed(&self) -> order::Model {
        self.order(Status::Completed).await
    }

    /// Handles an event of user `user_id`'s with `handle`, as the worker does. Returns the
    /// reply, which must fit LINE's limits if there is one.
    async fn handle(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<Vec<Message>, DbErr>,
    ) -> Vec<Message> {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let messages = handle(&turn).await.unwrap();
        let Finished {
            messages, outbox, ..
        } = turn.finish(messages);
        outbox
            .queue(&txn, &self.config.notification, t0())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        if !messages.is_empty() {
            limits::check(&messages).unwrap();
        }
        messages
    }

    /// User `user_id` taps a button sending `data`.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.handle(user_id, async |turn| {
            actions::tapped(turn, &self.config, &postback).await
        })
        .await
    }

    /// User `user_id` answers the step of rating they are on with `input`; `None` if the step
    /// doesn't take it.
    async fn sends(&self, user_id: i64, input: Input) -> Option<Vec<Message>> {
        let mut answered = None;
        self.handle(user_id, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<RatingFlow>());
            let session = session.expect("rating an order");
            answered = answer(turn, session, &input).await?;
            Ok(answered.clone().unwrap_or_default())
        })
        .await;
        answered
    }

    async fn types(&self, user_id: i64, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.sends(user_id, input).await.expect("an answer")
    }

    /// The buyer rates order `id` `stars`, then skips the comment.
    async fn rates(&self, id: i64, stars: i16) {
        self.taps(self.buyer, &star(id, stars)).await;
        self.types(self.buyer, "ข้าม").await;
    }

    /// The step of rating the user is on, if any.
    async fn step(&self, user_id: i64) -> Option<(Step, RatingFlow)> {
        let row = session::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .unwrap()?;
        assert_eq!(row.flow, RatingFlow::NAME);
        let step = serde_json::from_value(Json::String(row.step)).unwrap();
        Some((step, serde_json::from_value(row.data).unwrap()))
    }

    /// Order `id`'s rating, if it is rated, as (stars, comment).
    async fn rating(&self, id: i64) -> Option<(i16, Option<String>)> {
        of_order(&self.db, id)
            .await
            .unwrap()
            .map(|rated| (rated.stars, rated.comment))
    }

    async fn ratings(&self) -> u64 {
        rating::Entity::find().count(&self.db).await.unwrap()
    }

    /// Every push queued, as (type, recipient, messages), oldest first.
    async fn pushes(&self) -> Vec<(String, String, Json)> {
        push_log::Entity::find()
            .order_by_asc(push_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.push_type, row.recipient, row.messages.unwrap()))
            .collect()
    }

    /// User `user_id` is suspended.
    async fn suspends(&self, user_id: i64) {
        user::ActiveModel {
            id: Unchanged(user_id),
            status: Set(user::Status::Suspended),
            ..Default::default()
        }
        .update(&self.db)
        .await
        .unwrap();
    }

    /// The reply refusing what the actor asked, as `denied`.
    fn refused(&self, denied: Denied) -> Message {
        self.templates.refused(denied, None)
    }
}

/// The postback data of `action` on order `id`.
fn data(action: &str, id: i64) -> String {
    format!("{action}?order={id}")
}

/// The postback data of the Quick Reply giving order `id` `stars`.
fn star(id: i64, stars: i16) -> String {
    format!("{RATE}?order={id}&stars={stars}")
}

fn bubble(message: &Message) -> &Bubble {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Bubble(card),
            ..
        } => card,
        body => panic!("not a card: {body:?}"),
    }
}

/// The cards of a list.
fn cards(message: &Message) -> &[Bubble] {
    match &message.body {
        Body::Flex {
            contents: FlexContainer::Carousel(carousel),
            ..
        } => &carousel.contents,
        body => panic!("not a list: {body:?}"),
    }
}

/// Every text of `card`'s body, its rows' labels and values among them.
fn texts(card: &Bubble) -> Vec<&str> {
    fn walk<'a>(component: &'a Component, out: &mut Vec<&'a str>) {
        match component {
            Component::Text(text) => out.push(&text.text),
            Component::Box(flex_box) => {
                for component in &flex_box.contents {
                    walk(component, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for component in &card.body.as_ref().unwrap().contents {
        walk(component, &mut out);
    }
    out
}

/// The value of the row `label` of `card`, if it has one.
fn row<'a>(card: &'a Bubble, label: &str) -> Option<&'a str> {
    let texts = texts(card);
    let at = texts.iter().position(|text| *text == label)?;
    texts.get(at + 1).copied()
}

/// The buttons of a card, as (label, postback data).
fn buttons_of(card: &Bubble) -> Vec<(&str, &str)> {
    let Some(footer) = &card.footer else {
        return Vec::new();
    };
    footer
        .contents
        .iter()
        .map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => (label.as_str(), data.as_str()),
                action => panic!("not a postback: {action:?}"),
            },
            other => panic!("not a button: {other:?}"),
        })
        .collect()
}

/// The labels of a message's Quick Replies.
fn quick_replies(message: &Message) -> Vec<&str> {
    message
        .quick_reply
        .as_ref()
        .map(|quick_reply| quick_reply.items.iter())
        .into_iter()
        .flatten()
        .map(|item| item.action.label().as_str())
        .collect()
}

/// The postback data of a message's Quick Replies.
fn quick_reply_data(message: &Message) -> Vec<&str> {
    let items = message.quick_reply.as_ref().unwrap().items.iter();
    items
        .map(|item| match &item.action {
            Action::Postback { data, .. } => data.as_str(),
            action => panic!("not a postback: {action:?}"),
        })
        .collect()
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// Whether `buttons` offer to rate an order.
fn offers_rating(buttons: &[Action]) -> bool {
    buttons.iter().any(|action| match action {
        Action::Postback { data, .. } => data.starts_with(RATE),
        _ => false,
    })
}

/// FR-ORD-14: the buyer confirms receipt and gets "ให้คะแนน"; it asks how many stars, which are
/// kept as soon as they are tapped, then what to tell the shop, which is kept with them. Nothing
/// is pushed.
#[tokio::test]
async fn the_buyer_rates_a_completed_order() {
    let s = setup().await;
    let order = s.order(Status::Ready).await;
    let id = order.id;

    let received = s.taps(s.buyer, &data(RECEIVED, id)).await;
    let rate = data(RATE, id);
    assert_eq!(
        buttons_of(bubble(&received[0])),
        [("ให้คะแนน", rate.as_str())]
    );
    let pushed = s.pushes().await.len();

    let asked = s.taps(s.buyer, &rate).await;
    assert_eq!(
        text(&asked[0]),
        "ออเดอร์ IM-20261025-0001 จากร้านป้าแดง ถูกใจแค่ไหนคะ ให้คะแนน 1–5 ดาวได้เลยค่ะ"
    );
    assert_eq!(
        quick_replies(&asked[0]),
        ["★★★★★", "★★★★", "★★★", "★★", "★"]
    );
    let stars: Vec<String> = (1..=5).rev().map(|stars| star(id, stars)).collect();
    assert_eq!(quick_reply_data(&asked[0]), stars);
    assert_eq!(s.rating(id).await, None);
    assert_eq!(s.step(s.buyer).await, None);

    let asked = s.taps(s.buyer, &star(id, 4)).await;
    assert_eq!(
        text(&asked[0]),
        "ให้ออเดอร์ IM-20261025-0001 ★★★★☆ แล้วค่ะ อยากบอกอะไรถึงร้านไหมคะ พิมพ์ได้ไม่เกิน 200 ตัวอักษร ร้านจะเห็นในออเดอร์นี้ ถ้าไม่มีกด \"ข้าม\" ได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&asked[0]), ["ข้าม", "ยกเลิก"]);
    assert_eq!(s.rating(id).await, Some((4, None)));
    let flow = RatingFlow { order_id: id };
    assert_eq!(s.step(s.buyer).await, Some((Step::Comment, flow)));

    let thanked = s.types(s.buyer, " ไข่สดมากค่ะ \n").await;
    assert_eq!(text(&thanked[0]), "ขอบคุณที่ให้คะแนนออเดอร์ IM-20261025-0001ค่ะ");
    assert_eq!(quick_replies(&thanked[0]), ["เลือกซื้อต่อ", "เมนู"]);
    assert_eq!(s.rating(id).await, Some((4, Some("ไข่สดมากค่ะ".to_owned()))));
    assert_eq!(s.step(s.buyer).await, None);
    assert_eq!(s.pushes().await.len(), pushed);
}

/// FR-ORD-14, FR-ORD-08: the seller handing the order over tells the buyer with "ให้คะแนน"; the
/// seller is never offered it.
#[tokio::test]
async fn the_buyer_is_offered_a_rating_once_the_seller_hands_it_over() {
    let s = setup().await;
    let order = s.order(Status::Ready).await;
    let handed = s.taps(s.seller, &data(COMPLETE, order.id)).await;
    assert!(handed.iter().all(|message| {
        match &message.body {
            Body::Flex { .. } => buttons_of(bubble(message))
                .iter()
                .all(|(_, data)| !data.starts_with(RATE)),
            _ => true,
        }
    }));

    let pushes = s.pushes().await;
    let (_, to, messages) = pushes
        .iter()
        .find(|(kind, ..)| kind == "order.status")
        .expect("the buyer told");
    assert_eq!(to, BUYER);
    let buttons = messages[0]["contents"]["footer"]["contents"]
        .as_array()
        .unwrap();
    let rate = &buttons.last().unwrap()["action"];
    assert_eq!(rate["label"], "ให้คะแนน");
    assert_eq!(rate["data"], data(RATE, order.id));

    let completed = order::Model {
        status: Status::Completed,
        ..order
    };
    assert!(offers_rating(&actions::buttons(
        &completed,
        ActorType::Buyer
    )));
    assert!(!offers_rating(&actions::buttons(
        &completed,
        ActorType::Seller
    )));
    assert!(!offers_rating(&actions::buttons(
        &completed,
        ActorType::Admin
    )));
}

/// FR-ORD-14: only a completed order is rated. No other status offers it, and an old or forged
/// button shows the order as it is, keeping nothing.
#[tokio::test]
async fn only_a_completed_order_is_rated() {
    let s = setup().await;
    for status in [
        Status::Pending,
        Status::Accepted,
        Status::Ready,
        Status::Delivering,
        Status::Rejected,
        Status::Cancelled,
        Status::Expired,
    ] {
        let order = s.order(status).await;
        assert!(!offers_rating(&actions::buttons(&order, ActorType::Buyer)));
        let (number, name) = (&order.order_number, order_status_name(status));
        let now = format!("ออเดอร์ {number} ตอนนี้สถานะ \"{name}\"ค่ะ");
        for tapped in [data(RATE, order.id), star(order.id, 5)] {
            let shown = s.taps(s.buyer, &tapped).await;
            let headline = texts(bubble(&shown[0]))[0];
            assert_eq!(headline, now, "{status:?}");
            assert_eq!(s.step(s.buyer).await, None, "{status:?}");
        }
    }
    assert_eq!(s.ratings().await, 0);
}

/// FR-ORD-14 acceptance, DR-05: a second rating of the same order is refused, from "ให้คะแนน"
/// again or an old Quick Reply, and the first stays as it was.
#[tokio::test]
async fn a_second_rating_of_an_order_is_refused() {
    let s = setup().await;
    let order = s.completed().await;
    let id = order.id;
    s.taps(s.buyer, &star(id, 5)).await;
    s.types(s.buyer, "อร่อยค่ะ").await;

    let refused = "ออเดอร์ IM-20261025-0001 ให้คะแนนไปแล้ว (★★★★★) ให้ได้ออเดอร์ละครั้งเดียวค่ะ";
    for tapped in [data(RATE, id), star(id, 1), star(id, 5)] {
        let told = s.taps(s.buyer, &tapped).await;
        assert_eq!(text(&told[0]), refused, "{tapped}");
        assert_eq!(quick_replies(&told[0]), ["เลือกซื้อต่อ", "เมนู"]);
        assert_eq!(s.step(s.buyer).await, None);
    }
    assert_eq!(s.rating(id).await, Some((5, Some("อร่อยค่ะ".to_owned()))));
    assert_eq!(s.ratings().await, 1);
}

/// FR-ORD-14, DR-05: two stars tapped at the same moment keep one rating; the other tap is told
/// the order is rated already.
#[tokio::test]
async fn two_taps_at_once_keep_one_rating() {
    let s = setup().await;
    let order = s.completed().await;
    let id = order.id;
    let (five, one) = (star(id, 5), star(id, 1));
    let (first, second) = tokio::join!(s.taps(s.buyer, &five), s.taps(s.buyer, &one));

    assert_eq!(s.ratings().await, 1);
    let (stars, _) = s.rating(id).await.unwrap();
    let rated = format!(
        "ออเดอร์ IM-20261025-0001 ให้คะแนนไปแล้ว ({}) ให้ได้ออเดอร์ละครั้งเดียวค่ะ",
        templates::stars_line(stars)
    );
    let replies = [text(&first[0]), text(&second[0])];
    assert_eq!(
        replies.iter().filter(|reply| **reply == rated).count(),
        1,
        "{replies:?}"
    );
    assert_eq!(
        replies
            .iter()
            .filter(|reply| reply.starts_with("ให้ออเดอร์"))
            .count(),
        1,
        "{replies:?}"
    );
}

/// NFR-SEC-02, NFR-SEC-10: only the order's buyer rates it; a suspended buyer can't, even with
/// the comment half typed. Stars other than 1 to 5, and an order that isn't there, are refused.
#[tokio::test]
async fn only_the_buyer_rates_their_order() {
    let s = setup().await;
    let order = s.completed().await;
    let id = order.id;
    let not_allowed = s.refused(Denied::NotAllowed);
    for user_id in [s.seller, s.admin, s.other_seller, s.other_buyer] {
        for tapped in [data(RATE, id), star(id, 5)] {
            let refused = s.taps(user_id, &tapped).await;
            assert_eq!(
                refused,
                slice::from_ref(&not_allowed),
                "{tapped} by {user_id}"
            );
        }
    }
    let not_found = s.refused(Denied::NotFound);
    for tapped in [star(id, 0), star(id, 6), star(id, -1), data(RATE, i64::MAX)] {
        let refused = s.taps(s.buyer, &tapped).await;
        assert_eq!(refused, slice::from_ref(&not_found), "{tapped}");
    }
    assert_eq!(s.ratings().await, 0);

    // The buyer is suspended while typing
    s.taps(s.buyer, &star(id, 3)).await;
    s.suspends(s.buyer).await;
    let refused = s.types(s.buyer, "ช้าไปหน่อย").await;
    assert_eq!(refused, [s.refused(Denied::Suspended)]);
    assert_eq!(s.step(s.buyer).await, None);
    assert_eq!(s.rating(id).await, Some((3, None)));
    let refused = s.taps(s.buyer, &data(RATE, id)).await;
    assert_eq!(refused, [s.refused(Denied::Suspended)]);
}

/// FR-ORD-14: a comment is 1 to 200 characters, without the spaces around it; nothing, or more,
/// is asked for again, and the flow waits on. "ข้าม" keeps the stars alone.
#[tokio::test]
async fn a_comment_is_short_or_skipped() {
    let s = setup().await;
    let order = s.completed().await;
    let id = order.id;
    let asked = s.taps(s.buyer, &star(id, 2)).await;

    let again = s.types(s.buyer, " \n ").await;
    assert_eq!(text(&again[0]), text(&asked[0]));
    assert_eq!(quick_replies(&again[0]), ["ข้าม", "ยกเลิก"]);
    assert_eq!(s.step(s.buyer).await.unwrap().0, Step::Comment);

    let longest = "ก่".repeat(COMMENT_CHARS / 2);
    let too_long = s.types(s.buyer, &format!("{longest}ก")).await;
    assert_eq!(
        text(&too_long[0]),
        "ขอโทษค่ะ ข้อความยาวเกินไป พิมพ์ใหม่ให้ไม่เกิน 200 ตัวอักษรได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&too_long[0]), ["ข้าม", "ยกเลิก"]);
    assert_eq!(s.step(s.buyer).await.unwrap().0, Step::Comment);
    assert_eq!(s.rating(id).await, Some((2, None)));

    // Not text: routing goes on
    assert_eq!(s.sends(s.buyer, Input::Unsupported).await, None);

    s.types(s.buyer, &format!("  {longest}\n")).await;
    assert_eq!(s.step(s.buyer).await, None);
    assert_eq!(s.rating(id).await, Some((2, Some(longest))));

    // Skipped
    let skipped = s.completed().await;
    s.taps(s.buyer, &star(skipped.id, 5)).await;
    let thanked = s.types(s.buyer, "ข้าม").await;
    assert_eq!(text(&thanked[0]), "ขอบคุณที่ให้คะแนนออเดอร์ IM-20261025-0002ค่ะ");
    assert_eq!(s.step(s.buyer).await, None);
    assert_eq!(s.rating(skipped.id).await, Some((5, None)));
}

/// FR-ORD-14, FR-ORD-10: a rated order's cards show its stars and comment, to its buyer in
/// "คำสั่งซื้อของฉัน", to its seller in "ดูรายละเอียด" and to an admin who finds it, and no
/// longer offer "ให้คะแนน"; one yet to be rated still does.
#[tokio::test]
async fn a_rated_orders_cards_show_its_rating() {
    let s = setup().await;
    let rated = s.completed().await;
    let unrated = s.completed().await;
    s.taps(s.buyer, &star(rated.id, 4)).await;
    s.types(s.buyer, "ไข่สดมากค่ะ").await;
    let shown = "★★★★☆\n\"ไข่สดมากค่ะ\"";

    let mine = s
        .handle(s.buyer, async |turn| lists::typed(turn).await)
        .await;
    let mine = cards(&mine[0]);
    let (view_rated, view_unrated) = (data(VIEW, rated.id), data(VIEW, unrated.id));
    let card = |id: i64| {
        let view = data(VIEW, id);
        let shows = |card: &&Bubble| buttons_of(card).iter().any(|&(_, tap)| tap == view);
        mine.iter().find(shows).expect("a card of the order")
    };
    assert_eq!(row(card(rated.id), "คะแนน"), Some(shown));
    assert_eq!(
        buttons_of(card(rated.id)),
        [("ดูรายละเอียด", view_rated.as_str())]
    );
    assert_eq!(row(card(unrated.id), "คะแนน"), None);
    let rate = data(RATE, unrated.id);
    assert_eq!(
        buttons_of(card(unrated.id)),
        [
            ("ให้คะแนน", rate.as_str()),
            ("ดูรายละเอียด", view_unrated.as_str())
        ]
    );

    let view = Postback::parse(&view_rated).unwrap();
    for user_id in [s.buyer, s.seller] {
        let details = s
            .handle(user_id, async |turn| lists::view(turn, &view).await)
            .await;
        let card = bubble(&details[0]);
        assert_eq!(row(card, "คะแนน"), Some(shown), "{user_id}");
        assert!(
            buttons_of(card)
                .iter()
                .all(|(_, data)| !data.starts_with(RATE))
        );
    }
    let found = s
        .handle(s.admin, async |turn| lists::view(turn, &view).await)
        .await;
    let found = texts(bubble(&found[0]));
    assert!(found.contains(&"คะแนน: ★★★★☆\n\"ไข่สดมากค่ะ\""), "{found:?}");
}

/// FR-SHOP-09 acceptance: a shop's average shows only from its third rating, rounded to tenths;
/// the ratings of another shop's orders, and orders not rated, don't count.
#[tokio::test]
async fn a_shops_average_shows_from_its_third_rating() {
    let s = setup().await;
    let ids = [s.shop.id, s.other_shop.id];
    for stars in [5, 5] {
        let order = s.completed().await;
        s.rates(order.id, stars).await;
    }
    s.completed().await;
    for stars in [1, 1, 1] {
        let order = s
            .order_at(s.other_buyer, s.other_shop.id, Status::Completed)
            .await;
        let id = order.id;
        s.taps(s.other_buyer, &star(id, stars)).await;
        s.types(s.other_buyer, "ข้าม").await;
    }
    let shown = of_shops(&s.db, &ids).await.unwrap();
    assert_eq!(shown.get(&s.shop.id), None);
    let other = ShopRating {
        tenths: 10,
        count: 3,
    };
    assert_eq!(shown.get(&s.other_shop.id), Some(&other));

    let order = s.completed().await;
    s.rates(order.id, 4).await;
    let shown = of_shops(&s.db, &ids).await.unwrap();
    let average = ShopRating {
        tenths: 47,
        count: 3,
    };
    assert_eq!(shown.get(&s.shop.id), Some(&average));
    assert_eq!(of_shops(&s.db, &[]).await.unwrap(), HashMap::new());
}

/// FR-ORD-14: an average is rounded half up to one decimal.
#[test]
fn an_average_is_rounded_half_up_to_tenths() {
    for (count, stars, tenths) in [
        (3, 15, 50),
        (3, 14, 47),
        (3, 13, 43),
        (2, 9, 45),
        (20, 93, 47),
        (20, 91, 46),
        (7, 7, 10),
    ] {
        assert_eq!(
            average(count, stars),
            ShopRating { tenths, count },
            "{stars} stars of {count}"
        );
    }
}
