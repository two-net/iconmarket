//! "ลบบัญชี" (FR-ONB-07, AC-13): refused while orders are open, then confirmed once; every
//! personal-data field the entity models mark (DR-10) is cleared, the shop is closed for good
//! with its products deleted, and past orders stay with the deleted party anonymized. Each test
//! starts a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeDelta, TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, IdenStatic, Select, TransactionTrait};
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::dispatcher::Finished;
use crate::line::{Action, Body, limits};
use crate::notifications::templates::{DELETED_ACCOUNT, Templates};
use crate::orders::{self, lists};
use crate::persistence::entity::{audit_log, session};
use crate::persistence::{orm, random_id, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ALICE: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BOB: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const CAROL: &str = "Uc3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const DAVE: &str = "Ud4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let config = config::sample();
    Setup {
        _container: container,
        db: orm(pool),
        templates: Templates::new(&config.app),
    }
}

impl Setup {
    /// User `user_id` taps `postback` at `now`, as routing hands it over, in a transaction that
    /// commits once it is handled, with the user loaded afresh. Returns the reply, and the LINE
    /// users whose rich menus to unlink.
    async fn taps_at(
        &self,
        user_id: i64,
        postback: &Postback,
        now: DateTimeUtc,
    ) -> (Vec<Message>, Vec<String>) {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, now)
            .await
            .unwrap();
        let messages = match postback.action() {
            pending::CONFIRM => pending::tapped(&turn, postback).await,
            DELETE_ACCOUNT => tapped(&turn).await,
            orders::VIEW => lists::view(&turn, postback).await,
            other => panic!("not a button here: {other}"),
        };
        let Finished {
            messages, unlink, ..
        } = turn.finish(messages.unwrap());
        txn.commit().await.unwrap();
        limits::check(&messages).unwrap();
        (messages, unlink)
    }

    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.taps_at(user_id, &postback, t0()).await.0
    }

    /// User `user_id` taps "ลบบัญชี".
    async fn asks(&self, user_id: i64) -> Vec<Message> {
        self.taps(user_id, DELETE_ACCOUNT).await
    }

    /// User `user_id` taps "ลบบัญชี", and is asked to confirm: the confirm button's data.
    async fn asks_to_confirm(&self, user_id: i64) -> String {
        let ask = self.asks(user_id).await;
        quick_reply_data(&ask[0])[0]
            .expect("a confirm button")
            .to_owned()
    }

    /// User `user_id` sees order `order_id`, as JSON to look into.
    async fn sees_order(&self, user_id: i64, order_id: i64) -> String {
        let postback = Postback::new(orders::VIEW).with_int(orders::ORDER_ID, order_id);
        let (seen, _) = self.taps_at(user_id, &postback, t0()).await;
        serde_json::to_string(&seen).unwrap()
    }

    async fn user(&self, id: i64) -> user::Model {
        user::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn shop(&self, id: i64) -> shop::Model {
        shop::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn product(&self, id: i64) -> product::Model {
        product::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// A user following the OA, onboarded.
    async fn new_user(&self, line_user_id: &str) -> i64 {
        let mut user = testing::new_user(line_user_id);
        user.consent_version = Set(Some("1".to_owned()));
        user.consented_at = Set(Some(testing::at()));
        user.insert(&self.db).await.unwrap().id
    }

    /// An order `buyer_id` placed at `shop_id`, as it is now: `status`.
    async fn order(&self, buyer_id: i64, shop_id: i64, number: &str, status: order::Status) -> i64 {
        let mut order = testing::new_order(buyer_id, shop_id, number, random_id());
        order.status = Set(status);
        order.insert(&self.db).await.unwrap().id
    }

    async fn set_order_status(&self, id: i64, status: order::Status) {
        order::Entity::update_many()
            .col_expr(order::Column::Status, Expr::value(status))
            .filter(order::Column::Id.eq(id))
            .exec(&self.db)
            .await
            .unwrap();
    }
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The labels of `message`'s Quick Reply buttons.
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

/// The postback data of `message`'s Quick Reply buttons; `None` for those that aren't.
fn quick_reply_data(message: &Message) -> Vec<Option<&str>> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| match &item.action {
            Action::Postback { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect()
}

/// How many rows `query` finds.
async fn count<E: EntityTrait>(db: &DatabaseConnection, query: Select<E>) -> u64
where
    E::Model: Sync,
{
    query.count(db).await.unwrap()
}

/// The rows of `E` whose `column` is `id`.
fn of<E: EntityTrait>(column: E::Column, id: i64) -> Select<E> {
    E::find().filter(column.eq(id))
}

/// What [`everything`] made.
struct Everything {
    alice: i64,
    bob: i64,
    shop: i64,
    /// For sale, and deleted before
    products: [i64; 2],
    /// Alice's at Bob's shop, delivered, with its slip
    order: i64,
    slip: i64,
}

/// Alice, an admin and a seller, with every personal-data field of her own and her shop's, a
/// product for sale and one deleted before, an order she placed delivered to her with its slip,
/// and everything a user leaves on the way: a draft photo, a cart, a guided flow, a pending
/// action, a join-code attempt, AI memory, events she sent and notifications pushed to her. Bob
/// has some of it too, to be kept.
async fn everything(s: &Setup) -> Everything {
    let db = &s.db;
    let mut alice = testing::new_user(ALICE);
    alice.display_name = Set(Some("ป้าแดง".to_owned()));
    alice.picture_url = Set(Some("https://profile.line-scdn.net/0halice".to_owned()));
    alice.profile_refreshed_at = Set(Some(testing::at()));
    alice.is_admin = Set(true);
    alice.menu_roles = Set(Some(user::MenuRoles::SellerAdmin));
    alice.consent_version = Set(Some("1".to_owned()));
    alice.consented_at = Set(Some(testing::at()));
    alice.joined_at = Set(Some(testing::at()));
    alice.zone = Set(Some("ซอย 3".to_owned()));
    alice.house_number = Set(Some("45/2".to_owned()));
    alice.landmark_note = Set(Some("รั้วสีเขียว".to_owned()));
    alice.phone = Set(Some("0812345678".to_owned()));
    alice.latitude = Set(Some(13.75));
    alice.longitude = Set(Some(100.5));
    let alice = alice.insert(db).await.unwrap().id;

    let mut shop = testing::new_shop(alice);
    shop.phone = Set(Some("053123456".to_owned()));
    shop.promptpay_id = Set(Some("0899999999".to_owned()));
    shop.pickup_address = Set(Some("หน้าบ้านเลขที่ 45/2".to_owned()));
    shop.pickup_latitude = Set(Some(13.7));
    shop.pickup_longitude = Set(Some(100.6));
    let shop = shop.insert(db).await.unwrap().id;
    let eggs = testing::new_category("ไข่/นม").insert(db).await.unwrap().id;
    let for_sale = testing::new_product(shop, eggs, "ไข่ไก่", Some(8));
    let for_sale = for_sale.insert(db).await.unwrap().id;
    let mut gone = testing::new_product(shop, eggs, "ไข่เป็ด", None);
    gone.status = Set(product::Status::Deleted);
    gone.deleted_at = Set(Some(testing::at()));
    let gone = gone.insert(db).await.unwrap().id;
    s.photo(OwnerType::Product, for_sale).await;

    let bob = s.new_user(BOB).await;
    let bobs_shop = testing::new_shop(bob).insert(db).await.unwrap().id;
    let bobs_milk = testing::new_product(bobs_shop, eggs, "นมสด", None);
    let bobs_milk = bobs_milk.insert(db).await.unwrap().id;
    let mut order = testing::new_order(alice, bobs_shop, "IM-20261022-0001", random_id());
    order.status = Set(order::Status::Completed);
    order.fulfillment = Set(order::Fulfillment::Delivery);
    order.delivery_zone = Set(Some("ซอย 3".to_owned()));
    order.delivery_house_number = Set(Some("45/2".to_owned()));
    order.delivery_landmark_note = Set(Some("รั้วสีเขียว".to_owned()));
    order.delivery_latitude = Set(Some(13.75));
    order.delivery_longitude = Set(Some(100.5));
    order.note = Set(Some("ฝากไว้หน้าบ้าน".to_owned()));
    let order = order.insert(db).await.unwrap().id;
    let slip = payment_slip::ActiveModel {
        order_id: Set(order),
        image_id: Set(None),
        access_token: Set(random_id()),
        token_expires_at: Set(testing::at()),
        ai_reference: Set(Some("REF123".to_owned())),
        status: Set(payment_slip::Status::Confirmed),
        created_at: Set(testing::at()),
        ..Default::default()
    };
    let slip = slip.insert(db).await.unwrap().id;
    let slip_image = s.photo(OwnerType::Slip, slip).await;
    payment_slip::Entity::update_many()
        .col_expr(payment_slip::Column::ImageId, Expr::value(slip_image))
        .filter(payment_slip::Column::Id.eq(slip))
        .exec(db)
        .await
        .unwrap();

    for user_id in [alice, bob] {
        s.photo(OwnerType::Draft, user_id).await;
        let line = cart_item::ActiveModel {
            user_id: Set(user_id),
            product_id: Set(if user_id == alice {
                bobs_milk
            } else {
                for_sale
            }),
            quantity: Set(1),
            added_at: Set(testing::at()),
        };
        line.insert(db).await.unwrap();
        let flow = session::ActiveModel {
            user_id: Set(user_id),
            flow: Set("address".to_owned()),
            step: Set("zone".to_owned()),
            data: Set(json!({})),
            expires_at: Set(t0() + TimeDelta::minutes(10)),
        };
        flow.insert(db).await.unwrap();
        let pending = pending_action::ActiveModel {
            id: Set(random_id()),
            user_id: Set(user_id),
            kind: Set("product.delete".to_owned()),
            payload: Set(json!({ "product_id": for_sale })),
            expires_at: Set(t0() + TimeDelta::minutes(10)),
            consumed_at: Set(None),
            created_at: Set(testing::at()),
        };
        pending.insert(db).await.unwrap();
        let attempt = join_attempt::ActiveModel {
            user_id: Set(user_id),
            at: Set(testing::at()),
            ..Default::default()
        };
        attempt.insert(db).await.unwrap();
        let memory = ai_message::ActiveModel {
            user_id: Set(user_id),
            role: Set(ai_message::Role::User),
            content: Set("มีไข่ไหม".to_owned()),
            created_at: Set(testing::at()),
            ..Default::default()
        };
        memory.insert(db).await.unwrap();
    }

    for (line_user_id, event_id, status) in [
        (ALICE, "e1", webhook_event::Status::Done),
        (ALICE, "e2", webhook_event::Status::Failed),
        (ALICE, "e3", webhook_event::Status::Pending),
        (BOB, "e4", webhook_event::Status::Done),
    ] {
        let said =
            json!({ "webhookEventId": event_id, "type": "message", "message": { "text": "ขอไข่" } });
        let mut event = testing::new_event(line_user_id, said, testing::at());
        event.status = Set(status);
        event.insert(db).await.unwrap();
    }
    for recipient in [ALICE, BOB] {
        let push = push_log::ActiveModel {
            push_type: Set("ORDER_STATUS".to_owned()),
            recipient: Set(recipient.to_owned()),
            status: Set(push_log::Status::Sent),
            sent_at: Set(testing::at()),
            messages: Set(Some(
                json!([{ "type": "text", "text": "ร้านรับออเดอร์แล้วค่ะ" }]),
            )),
            attempts: Set(1),
            queued_at: Set(testing::at()),
            ..Default::default()
        };
        push.insert(db).await.unwrap();
    }
    Everything {
        alice,
        bob,
        shop,
        products: [for_sale, gone],
        order,
        slip,
    }
}

impl Setup {
    /// An image of `owner_type` owned by `owner_id`: its ID.
    async fn photo(&self, owner_type: OwnerType, owner_id: i64) -> Uuid {
        let image = image::ActiveModel {
            id: Set(random_id()),
            owner_type: Set(owner_type),
            owner_id: Set(owner_id),
            position: Set(0),
            content_type: Set("image/jpeg".to_owned()),
            width: Set(1),
            height: Set(1),
            size_bytes: Set(1),
            data: Set(vec![0xff]),
            created_at: Set(testing::at()),
        };
        image.insert(&self.db).await.unwrap().id
    }
}

/// AC-13: a user with an open order, as buyer or at their shop, is refused with an
/// explanation and where to see those orders, and nothing changes; once none is open, they
/// are asked to confirm.
#[tokio::test]
async fn an_open_order_keeps_the_account() {
    let s = setup().await;
    let t = &s.templates;
    let alice = s.new_user(ALICE).await;
    let bob = s.new_user(BOB).await;
    let alices_shop = testing::new_shop(alice).insert(&s.db).await.unwrap().id;
    let bobs_shop = testing::new_shop(bob).insert(&s.db).await.unwrap().id;
    let bought = s
        .order(alice, bobs_shop, "IM-20261024-0001", order::Status::Pending)
        .await;
    let sold = s
        .order(
            bob,
            alices_shop,
            "IM-20261024-0002",
            order::Status::Delivering,
        )
        .await;

    let refused = s.asks(alice).await;
    assert_eq!(refused, [t.account_deletion_refused(1, 1)]);
    assert_eq!(
        text(&refused[0]),
        "ยังลบบัญชีไม่ได้ค่ะ เพราะมีคำสั่งซื้อที่ยังไม่จบ: ที่คุณสั่งซื้อ 1 รายการ และ ของร้านคุณ 1 รายการ เมื่อคำสั่งซื้อทั้งหมดเสร็จหรือยกเลิกแล้ว กด \"ลบบัญชี\" ได้อีกครั้งค่ะ"
    );
    assert_eq!(
        quick_replies(&refused[0]),
        ["คำสั่งซื้อของฉัน", "ออเดอร์ร้าน", "เมนู"]
    );

    // Each in turn, as the other finishes
    s.set_order_status(bought, order::Status::Completed).await;
    let refused = s.asks(alice).await;
    assert_eq!(refused, [t.account_deletion_refused(0, 1)]);
    assert_eq!(quick_replies(&refused[0]), ["ออเดอร์ร้าน", "เมนู"]);
    s.set_order_status(bought, order::Status::Accepted).await;
    s.set_order_status(sold, order::Status::Cancelled).await;
    let refused = s.asks(alice).await;
    assert_eq!(quick_replies(&refused[0]), ["คำสั่งซื้อของฉัน", "เมนู"]);
    let pending = of::<pending_action::Entity>(pending_action::Column::UserId, alice);
    assert_eq!(count(&s.db, pending).await, 0);

    // Unpaid or owed, but over: not open (SRS §1.3)
    s.set_order_status(bought, order::Status::Completed).await;
    let ask = s.asks(alice).await;
    assert_eq!(quick_replies(&ask[0]), ["ยืนยันลบ", "ยกเลิก"]);
    assert!(text(&ask[0]).contains("ร้าน \"ร้านป้าแดง\" จะปิดถาวรและสินค้าทั้งหมดถูกลบ"));

    let user = s.user(alice).await;
    assert_eq!(
        (user.status, user.line_user_id.as_deref()),
        (user::Status::Active, Some(ALICE))
    );
    assert_eq!(s.shop(alices_shop).await.status, shop::Status::Approved);
}

/// FR-ONB-07, DR-04, DR-10, NFR-PRIV-03: confirmed, the account is deleted: every field the
/// entity models mark as personal data is cleared from the user, their shop, the orders they
/// placed and their slips, and their consent is withdrawn; the shop is closed for good and its
/// products deleted, the rows kept; what they had in progress and the logs holding their LINE
/// user ID go, but not the events still to handle; it is audited, and their rich menu is
/// unlinked. Nobody else's is touched.
#[tokio::test]
async fn deleting_the_account_clears_their_personal_data() {
    let s = setup().await;
    let t = &s.templates;
    let db = &s.db;
    let e = everything(&s).await;

    let ask = s.asks(e.alice).await;
    let confirm = quick_reply_data(&ask[0])[0].unwrap().to_owned();
    let id = Postback::parse(&confirm).unwrap().uuid("pa").unwrap();
    let button = Postback::new(pending::CONFIRM)
        .with_name("k", DeleteAccount::KIND)
        .with_uuid("pa", id)
        .button(labels::CONFIRM_DELETE.clone());
    assert_eq!(ask, [t.confirm_delete_account(Some("ร้านป้าแดง"), button)]);
    assert_eq!(quick_replies(&ask[0]), ["ยืนยันลบ", "ยกเลิก"]);
    assert_eq!(s.user(e.alice).await.status, user::Status::Active);

    let confirmed = Postback::parse(&confirm).unwrap();
    let (deleted, unlink) = s.taps_at(e.alice, &confirmed, t0()).await;
    assert_eq!(deleted, [t.account_deleted()]);
    assert_eq!(
        text(&deleted[0]),
        "ลบบัญชีของคุณแล้วค่ะ ขอบคุณที่ใช้ IconMarket ถ้าอยากกลับมาใช้อีก ส่งข้อความมาได้เลย ระบบจะเริ่มให้ใหม่ค่ะ"
    );
    assert!(deleted[0].quick_reply.is_none());
    assert_eq!(unlink, [ALICE]);

    // Every personal-data field, and her consent and roles
    for &column in user::Entity::FIELDS.iter().chain(&WITHDRAWN) {
        let left = of::<user::Entity>(user::Column::Id, e.alice).filter(column.is_not_null());
        assert_eq!(
            count(db, left).await,
            0,
            "users.{} is left",
            column.as_str()
        );
    }
    for &column in shop::Entity::FIELDS {
        let left = of::<shop::Entity>(shop::Column::Id, e.shop).filter(column.is_not_null());
        assert_eq!(
            count(db, left).await,
            0,
            "shops.{} is left",
            column.as_str()
        );
    }
    for &column in order::Entity::FIELDS {
        let left = of::<order::Entity>(order::Column::Id, e.order).filter(column.is_not_null());
        assert_eq!(
            count(db, left).await,
            0,
            "orders.{} is left",
            column.as_str()
        );
    }
    for &column in payment_slip::Entity::FIELDS {
        let left = of::<payment_slip::Entity>(payment_slip::Column::Id, e.slip)
            .filter(column.is_not_null());
        let column = column.as_str();
        assert_eq!(count(db, left).await, 0, "payment_slips.{column} is left");
    }
    let alice = s.user(e.alice).await;
    assert_eq!(
        (
            alice.status,
            alice.is_admin,
            alice.is_following,
            alice.updated_at
        ),
        (user::Status::Deleted, false, false, t0())
    );

    // The shop closed for good, its name kept; its products deleted, the rows kept
    let shop = s.shop(e.shop).await;
    assert_eq!(
        (
            shop.status,
            shop.is_open,
            shop.deleted_at,
            shop.name.as_str()
        ),
        (shop::Status::Closed, false, Some(t0()), "ร้านป้าแดง")
    );
    let [for_sale, gone] = e.products;
    let for_sale = s.product(for_sale).await;
    assert_eq!(
        (for_sale.status, for_sale.deleted_at),
        (product::Status::Deleted, Some(t0()))
    );
    assert_eq!(s.product(gone).await.deleted_at, Some(testing::at()));

    // Her order stays, with its note, and so does its slip but for its image
    let order = order::Entity::find_by_id(e.order)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (order.status, order.buyer_id, order.note.as_deref()),
        (order::Status::Completed, e.alice, Some("ฝากไว้หน้าบ้าน"))
    );
    let slip = payment_slip::Entity::find_by_id(e.slip)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(slip.status, payment_slip::Status::Confirmed);
    let images = |owner_type: OwnerType, owner_id: i64| {
        of::<image::Entity>(image::Column::OwnerId, owner_id)
            .filter(image::Column::OwnerType.eq(owner_type))
    };
    assert_eq!(count(db, images(OwnerType::Slip, e.slip)).await, 0);
    // The products' photos go with retention
    assert_eq!(count(db, images(OwnerType::Product, for_sale.id)).await, 1);

    // What she had in progress
    for (user_id, expected) in [(e.alice, 0), (e.bob, 1)] {
        let counts = [
            count(
                db,
                of::<cart_item::Entity>(cart_item::Column::UserId, user_id),
            )
            .await,
            count(db, of::<session::Entity>(session::Column::UserId, user_id)).await,
            count(
                db,
                of::<pending_action::Entity>(pending_action::Column::UserId, user_id),
            )
            .await,
            count(
                db,
                of::<join_attempt::Entity>(join_attempt::Column::UserId, user_id),
            )
            .await,
            count(
                db,
                of::<ai_message::Entity>(ai_message::Column::UserId, user_id),
            )
            .await,
            count(db, images(OwnerType::Draft, user_id)).await,
        ];
        assert_eq!(counts, [expected; 6], "user {user_id}");
    }

    // The logs holding her LINE user ID, but the event still to handle
    let events: Vec<(String, String)> = webhook_event::Entity::find()
        .select_only()
        .column(webhook_event::Column::UserKey)
        .column(webhook_event::Column::EventId)
        .order_by_asc(webhook_event::Column::Id)
        .into_tuple()
        .all(db)
        .await
        .unwrap();
    let events: Vec<_> = events
        .iter()
        .map(|(key, id)| (key.as_str(), id.as_str()))
        .collect();
    assert_eq!(events, [(ALICE, "e3"), (BOB, "e4")]);
    let pushes: Vec<String> = push_log::Entity::find()
        .select_only()
        .column(push_log::Column::Recipient)
        .into_tuple()
        .all(db)
        .await
        .unwrap();
    assert_eq!(pushes, [BOB]);

    // Audited, by her, without naming her
    let audited = audit_log::Entity::find().all(db).await.unwrap();
    let audited: Vec<_> = audited
        .iter()
        .map(|entry| {
            let target = (entry.target_type.as_str(), entry.target_id);
            (
                entry.actor_id,
                entry.action.as_str(),
                target,
                &entry.details,
            )
        })
        .collect();
    let details = json!({ "shop_id": e.shop, "products_deleted": 1 });
    assert_eq!(
        audited,
        [(
            Some(e.alice),
            "user.deleted",
            ("user", Some(e.alice)),
            &details
        )]
    );

    // Bob is untouched
    let bob = s.user(e.bob).await;
    assert_eq!(
        (bob.status, bob.line_user_id.as_deref()),
        (user::Status::Active, Some(BOB))
    );
}

/// FR-UI-13: an order placed at the shop between the question and the confirmation keeps the
/// account, and the refusal says why; so does a shop applied for meanwhile, which is asked
/// about again.
#[tokio::test]
async fn what_changed_since_the_question_deletes_nothing() {
    let s = setup().await;
    let t = &s.templates;
    let alice = s.new_user(ALICE).await;
    let bob = s.new_user(BOB).await;
    let shop = testing::new_shop(alice).insert(&s.db).await.unwrap().id;

    let confirm = s.asks_to_confirm(alice).await;
    let order = s
        .order(bob, shop, "IM-20261024-0001", order::Status::Pending)
        .await;
    let refused = s.taps(alice, &confirm).await;
    assert_eq!(refused, [t.account_deletion_refused(0, 1)]);
    let user = s.user(alice).await;
    assert_eq!(
        (user.status, user.line_user_id.as_deref()),
        (user::Status::Active, Some(ALICE))
    );
    assert_eq!(s.shop(shop).await.status, shop::Status::Approved);

    // Asked without a shop, confirmed with one: asked again, naming it
    s.set_order_status(order, order::Status::Rejected).await;
    let carol = s.new_user(CAROL).await;
    let confirm = s.asks_to_confirm(carol).await;
    let mut applied = testing::new_shop(carol);
    applied.name = Set("ร้านขนมบ้านสวน".to_owned());
    applied.status = Set(shop::Status::Pending);
    let applied = applied.insert(&s.db).await.unwrap().id;
    let again = s.taps(carol, &confirm).await;
    assert!(text(&again[0]).contains("ร้าน \"ร้านขนมบ้านสวน\" จะปิดถาวร"));
    assert_eq!(quick_replies(&again[0]), ["ยืนยันลบ", "ยกเลิก"]);
    assert_eq!(s.user(carol).await.status, user::Status::Active);

    // Confirmed this time: the application is closed too
    let confirm = quick_reply_data(&again[0])[0].unwrap().to_owned();
    assert_eq!(s.taps(carol, &confirm).await, [t.account_deleted()]);
    assert_eq!(s.shop(applied).await.status, shop::Status::Closed);
}

/// FR-UI-13: a confirmation tapped once it expired, or after "ยกเลิก", deletes nothing, and
/// "ลบบัญชี" asks again.
#[tokio::test]
async fn an_expired_confirmation_keeps_the_account() {
    let s = setup().await;
    let t = &s.templates;
    let alice = s.new_user(ALICE).await;

    let confirm = Postback::parse(&s.asks_to_confirm(alice).await).unwrap();
    let late = t0() + TimeDelta::minutes(61);
    let (kept, unlink) = s.taps_at(alice, &confirm, late).await;
    let again = Postback::new(DELETE_ACCOUNT).button(labels::DELETE_ACCOUNT.clone());
    assert_eq!(kept, [t.account_kept(again)]);
    assert_eq!(
        text(&kept[0]),
        "ยังไม่ได้ลบบัญชีค่ะ ถ้าจะลบ กด \"ลบบัญชี\" อีกครั้งได้เลยค่ะ"
    );
    assert_eq!(quick_replies(&kept[0]), ["ลบบัญชี", "เมนู"]);
    assert_eq!(quick_reply_data(&kept[0])[0], Some(DELETE_ACCOUNT));
    assert!(unlink.is_empty());
    assert_eq!(s.user(alice).await.status, user::Status::Active);

    let confirm = s.asks_to_confirm(alice).await;
    pending::discard(&s.db, alice).await.unwrap();
    assert_eq!(s.taps(alice, &confirm).await[0], kept[0]);
    assert_eq!(s.user(alice).await.status, user::Status::Active);
}

/// NFR-PRIV-03, FR-ADM-04: a suspended user keeps the right to delete their account. A buyer
/// with no menu of their own has none to unlink.
#[tokio::test]
async fn a_suspended_user_can_delete_their_account() {
    let s = setup().await;
    let alice = s.new_user(ALICE).await;
    user::Entity::update_many()
        .col_expr(user::Column::Status, Expr::value(user::Status::Suspended))
        .col_expr(user::Column::SuspensionReason, Expr::value("โกงลูกค้า"))
        .filter(user::Column::Id.eq(alice))
        .exec(&s.db)
        .await
        .unwrap();

    let ask = s.asks(alice).await;
    assert!(text(&ask[0]).starts_with("ลบบัญชีของคุณใช่ไหมคะ ระบบจะลบชื่อ"));
    let confirm = Postback::parse(quick_reply_data(&ask[0])[0].unwrap()).unwrap();
    let (deleted, unlink) = s.taps_at(alice, &confirm, t0()).await;
    assert_eq!(deleted, [s.templates.account_deleted()]);
    assert!(unlink.is_empty());
    let user = s.user(alice).await;
    assert_eq!(
        (user.status, user.suspension_reason),
        (user::Status::Deleted, None)
    );
}

/// FR-ONB-07, DR-04: past orders stay for the other party: the seller and an admin see the
/// buyer who deleted their account as such, and a buyer still sees the closed shop by name.
#[tokio::test]
async fn past_orders_show_the_deleted_party() {
    let s = setup().await;
    let mut alice = testing::new_user(ALICE);
    alice.display_name = Set(Some("สมศรี".to_owned()));
    alice.consent_version = Set(Some("1".to_owned()));
    let alice = alice.insert(&s.db).await.unwrap().id;
    let bob = s.new_user(BOB).await;
    let carol = s.new_user(CAROL).await;
    let mut dave = testing::new_user(DAVE);
    dave.is_admin = Set(true);
    dave.consent_version = Set(Some("1".to_owned()));
    let dave = dave.insert(&s.db).await.unwrap().id;
    let mut alices_shop = testing::new_shop(alice);
    alices_shop.name = Set("ร้านขนมบ้านสวน".to_owned());
    let alices_shop = alices_shop.insert(&s.db).await.unwrap().id;
    let bobs_shop = testing::new_shop(bob).insert(&s.db).await.unwrap().id;
    let bought = s
        .order(
            alice,
            bobs_shop,
            "IM-20261024-0001",
            order::Status::Completed,
        )
        .await;
    let sold = s
        .order(
            carol,
            alices_shop,
            "IM-20261024-0002",
            order::Status::Completed,
        )
        .await;
    assert!(s.sees_order(bob, bought).await.contains("สมศรี"));

    let confirm = s.asks_to_confirm(alice).await;
    assert_eq!(
        s.taps(alice, &confirm).await,
        [s.templates.account_deleted()]
    );

    let seller_sees = s.sees_order(bob, bought).await;
    assert!(seller_sees.contains(DELETED_ACCOUNT), "{seller_sees}");
    assert!(!seller_sees.contains("สมศรี"));
    let admin_sees = s.sees_order(dave, bought).await;
    assert!(
        admin_sees.contains(&format!("ลูกค้า: {DELETED_ACCOUNT}")),
        "{admin_sees}"
    );
    let buyer_sees = s.sees_order(carol, sold).await;
    assert!(buyer_sees.contains("ร้านขนมบ้านสวน"), "{buyer_sees}");
}
