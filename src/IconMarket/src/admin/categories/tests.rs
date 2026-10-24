//! "หมวดหมู่" (FR-ADM-05, FR-ADM-08): who may use it, what each button and the name flow do,
//! that a stale tap changes nothing, that a category with products is never deleted, and that
//! every change is audited. Each event is handled in a transaction committed once it is
//! handled. The `async` tests each start a throwaway PostgreSQL, so they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::prelude::{DateTimeUtc, Json};
use sea_orm::{DatabaseConnection, QueryOrder, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, SessionConfig};
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Action, Body, QuickReply, limits};
use crate::notifications::templates;
use crate::persistence::entity::{audit_log, pending_action, shop, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const ADMIN: &str = "Ua1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BUYER: &str = "Ub2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

#[test]
fn names_are_tidied_and_bounded() {
    assert_eq!(name("  ขนม   ไทย \n").as_deref(), Ok("ขนม ไทย"));
    assert_eq!(name("ผัก/ผลไม้").as_deref(), Ok("ผัก/ผลไม้"));
    let longest = "ข".repeat(NAME_CHARS);
    assert_eq!(name(&longest), Ok(longest.clone()));
    assert_eq!(
        name(&format!("{longest}ข")),
        Err(Refused::TooLong(NAME_CHARS))
    );
    assert_eq!(name(" \t "), Err(Refused::Empty));
    // What fits a name fits a button
    assert_eq!(NAME_CHARS, Label::MAX_CHARS);
}

#[test]
fn a_move_swaps_with_the_neighbour_it_was_sent_for() {
    let order = [1, 2, 3];
    assert_eq!(swapped(&order, 2, Direction::Up, 1), Some(vec![2, 1, 3]));
    assert_eq!(swapped(&order, 2, Direction::Down, 3), Some(vec![1, 3, 2]));
    assert_eq!(swapped(&order, 1, Direction::Down, 2), Some(vec![2, 1, 3]));
    assert_eq!(swapped(&order, 3, Direction::Up, 2), Some(vec![1, 3, 2]));
    // The first can't go up, nor the last down
    assert_eq!(swapped(&order, 1, Direction::Up, 3), None);
    assert_eq!(swapped(&order, 3, Direction::Down, 1), None);
    // The neighbour changed since, or the category is gone
    assert_eq!(swapped(&order, 3, Direction::Up, 1), None);
    assert_eq!(swapped(&order, 4, Direction::Up, 3), None);
    // A second tap on the same button
    let once = swapped(&order, 2, Direction::Up, 1).unwrap();
    assert_eq!(swapped(&once, 2, Direction::Up, 1), None);
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    templates: Templates,
    admin: i64,
    buyer: i64,
}

/// An admin and a buyer, and no categories.
async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let db = orm(pool);
    let admin = user::ActiveModel {
        is_admin: Set(true),
        ..testing::new_user(ADMIN)
    };
    let admin = admin.insert(&db).await.unwrap().id;
    let buyer = testing::new_user(BUYER).insert(&db).await.unwrap().id;
    Setup {
        _container: container,
        db,
        templates: Templates::new(&config::sample().app),
        admin,
        buyer,
    }
}

impl Setup {
    /// Handles an event of user `user_id`'s at `t0()` with `handle`, as the worker does: in a
    /// transaction that commits once it is handled, with the user loaded afresh.
    async fn at<T>(
        &self,
        user_id: i64,
        handle: impl AsyncFnOnce(&Turn<'_>) -> Result<T, DbErr>,
    ) -> T {
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let result = handle(&turn).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        result
    }

    /// User `user_id` taps a button sending `data`, as routing hands it over.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        self.at(user_id, async |turn| {
            if postback.action() == pending::CONFIRM {
                pending::tapped(turn, &postback).await
            } else {
                tapped(turn, &postback).await
            }
        })
        .await
    }

    /// The admin types `text` in their category flow.
    async fn types(&self, text: &str) -> Vec<Message> {
        let input = Input::Text(text.to_owned());
        self.at(self.admin, async |turn| {
            let session = turn
                .flow
                .as_ref()
                .and_then(|flow| flow.read::<CategoryFlow>());
            answer(turn, session.expect("in the category flow"), &input).await
        })
        .await
        .expect("an answer")
    }

    /// Whether the admin is still asked for a name.
    async fn naming(&self) -> bool {
        self.at(self.admin, async |turn| Ok(turn.flow.is_some()))
            .await
    }

    /// "หมวดหมู่" as the admin sees it now, at the page showing `around`.
    async fn list(&self, around: Option<i64>) -> Message {
        list(&self.db, &self.templates, 0, around).await.unwrap()
    }

    /// Adds categories called `names`, active, in that order after any there are.
    async fn add(&self, names: &[&str]) -> Vec<category::Model> {
        let last = self.categories().await.len() as i32;
        let mut added = Vec::new();
        for (name, sort_order) in names.iter().zip(last + 1..) {
            let category = category::ActiveModel {
                sort_order: Set(sort_order),
                ..testing::new_category(name)
            };
            added.push(category.insert(&self.db).await.unwrap());
        }
        added
    }

    /// Lists a product called `name` with `status` in category `category_id`.
    async fn list_product(&self, category_id: i64, name: &str, status: product::Status) {
        let owner = testing::new_user(&format!("U-seller-{name}"))
            .insert(&self.db)
            .await
            .unwrap();
        let shop = shop::ActiveModel {
            name: Set(format!("ร้าน{name}")),
            ..testing::new_shop(owner.id)
        };
        let shop = shop.insert(&self.db).await.unwrap();
        let product = product::ActiveModel {
            status: Set(status),
            ..testing::new_product(shop.id, category_id, name, None)
        };
        product.insert(&self.db).await.unwrap();
    }

    /// Every category as (name, place, active), in order.
    async fn categories(&self) -> Vec<(String, i32, bool)> {
        categories::ordered()
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|category| (category.name, category.sort_order, category.is_active))
            .collect()
    }

    async fn active(&self) -> Vec<String> {
        categories::active(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|category| category.name)
            .collect()
    }

    /// Every audit row as (actor, action, target type, target, details), oldest first.
    async fn audited(&self) -> Vec<(Option<i64>, String, String, Option<i64>, Json)> {
        audit_log::Entity::find()
            .order_by_asc(audit_log::Column::Id)
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                let created_at = row.created_at;
                assert_eq!(created_at, t0());
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

    /// The audit row of the admin doing `action` to category `id`, with `details`.
    fn by_admin(
        &self,
        action: &str,
        id: i64,
        details: Json,
    ) -> (Option<i64>, String, String, Option<i64>, Json) {
        let (action, target_type) = (action.to_owned(), "category".to_owned());
        (Some(self.admin), action, target_type, Some(id), details)
    }

    /// `ask` as a prompt of the name flow is sent: with the cancel button.
    fn prompt(&self, ask: Message) -> Message {
        templates::with_cancel(ask)
    }
}

/// The cards of a "หมวดหมู่" page: each one's title and the label and data of each of its
/// buttons; "ดูเพิ่มเติม" is a card without a title.
fn cards(message: &Message) -> Vec<(String, Vec<(String, String)>)> {
    let Body::Flex {
        contents: FlexContainer::Carousel(carousel),
        ..
    } = &message.body
    else {
        panic!("not a carousel: {message:?}");
    };
    let buttons = |component: &Component| -> Vec<(String, String)> {
        let row = match component {
            Component::Box(row) => row.contents.as_slice(),
            component => std::slice::from_ref(component),
        };
        row.iter()
            .map(|component| match component {
                Component::Button(button) => match &button.action {
                    Action::Postback { label, data, .. } => {
                        (label.as_str().to_owned(), data.clone())
                    }
                    action => panic!("not a postback: {action:?}"),
                },
                component => panic!("not a button: {component:?}"),
            })
            .collect()
    };
    carousel
        .contents
        .iter()
        .map(|bubble| {
            let title = match bubble.header.as_ref().map(|h| h.contents.as_slice()) {
                Some([Component::Text(text)]) => text.text.clone(),
                None => String::new(),
                header => panic!("no title: {header:?}"),
            };
            // A card's buttons are in its footer, "ดูเพิ่มเติม" in its body
            let rows = bubble.footer.as_ref().or(bubble.body.as_ref());
            let rows = rows.map_or(&[][..], |rows| rows.contents.as_slice());
            (title, rows.iter().flat_map(buttons).collect())
        })
        .collect()
}

fn titles(message: &Message) -> Vec<String> {
    cards(message).into_iter().map(|(title, _)| title).collect()
}

/// The data of the button labelled `label` on the card titled `title`.
fn button(message: &Message, title: &str, label: &Label) -> String {
    let cards = cards(message);
    let (_, buttons) = cards
        .iter()
        .find(|(t, _)| t == title)
        .unwrap_or_else(|| panic!("no card {title:?} in {cards:?}"));
    buttons
        .iter()
        .find(|(l, _)| l == label.as_str())
        .unwrap_or_else(|| panic!("no {label:?} on {title:?}: {buttons:?}"))
        .1
        .clone()
}

/// The data of the Quick Reply postback of `message`.
fn quick_postback(message: &Message) -> String {
    let items = &message.quick_reply.as_ref().expect("a Quick Reply").items;
    items
        .iter()
        .find_map(|item| match &item.action {
            Action::Postback { data, .. } => Some(data.clone()),
            _ => None,
        })
        .expect("a postback")
}

/// FR-ADM-05, NFR-SEC-02: only admins see or change categories; a buyer's taps, old buttons
/// included, change nothing.
#[tokio::test]
async fn only_admins_manage_categories() {
    let s = setup().await;
    let t = &s.templates;
    let [eggs] = s.add(&["ไข่/นม"]).await.try_into().unwrap();
    let refused = vec![t.denied(Denied::NotAllowed)];
    for data in [
        "categories".to_owned(),
        ADD.to_owned(),
        format!("{RENAME}?c={}", eggs.id),
        format!("{DEACTIVATE}?c={}", eggs.id),
        format!("{DELETE}?c={}", eggs.id),
    ] {
        assert_eq!(s.taps(s.buyer, &data).await, refused, "{data}");
    }
    let flows = s.at(s.buyer, async |turn| Ok(turn.flow.is_some())).await;
    assert!(!flows, "no flow started");
    assert_eq!(s.categories().await, [("ไข่/นม".to_owned(), 1, true)]);
    assert_eq!(s.audited().await, []);
    assert_eq!(pending_action::Entity::find().all(&s.db).await.unwrap(), []);
}

/// "หมวดหมู่" shows every category in order, a card each, with the buttons that apply.
#[tokio::test]
async fn the_list_shows_every_category_with_its_buttons() {
    let s = setup().await;
    let [food, sweets, eggs] = s.add(&["อาหาร", "ขนม", "ไข่/นม"]).await.try_into().unwrap();
    s.list_product(sweets.id, "ขนมครก", product::Status::Active)
        .await;
    s.list_product(sweets.id, "ขนมชั้น", product::Status::Deleted)
        .await;
    s.taps(s.admin, &format!("{DEACTIVATE}?c={}", eggs.id))
        .await;

    let list = s.taps(s.admin, "categories").await;
    assert_eq!(list.len(), 1);
    let list = &list[0];
    limits::check(std::slice::from_ref(list)).unwrap();
    assert_eq!(titles(list), ["1. อาหาร", "2. ขนม", "3. ไข่/นม"]);
    let labels_on = |title| -> Vec<String> {
        cards(list)
            .into_iter()
            .find(|(t, _)| t == title)
            .unwrap()
            .1
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    };
    assert_eq!(
        labels_on("1. อาหาร"),
        ["เลื่อนลง", "เปลี่ยนชื่อ", "ปิดใช้งาน", "ลบ"]
    );
    assert_eq!(
        labels_on("2. ขนม"),
        ["เลื่อนขึ้น", "เลื่อนลง", "เปลี่ยนชื่อ", "ปิดใช้งาน", "ลบ"]
    );
    assert_eq!(
        labels_on("3. ไข่/นม"),
        ["เลื่อนขึ้น", "เปลี่ยนชื่อ", "เปิดใช้งาน", "ลบ"]
    );
    assert_eq!(
        button(list, "2. ขนม", &labels::MOVE_UP),
        format!("{UP}?c={}&n={}", sweets.id, food.id)
    );
    assert_eq!(
        button(list, "2. ขนม", &labels::MOVE_DOWN),
        format!("{DOWN}?c={}&n={}", sweets.id, eggs.id)
    );
    // Deleted products aren't counted
    let text = serde_json::to_string(list).unwrap();
    assert!(text.contains("1 รายการ"), "{text}");
    assert!(text.contains("ปิดใช้ (ผู้ซื้อไม่เห็น)"), "{text}");
    let add = Postback::new(ADD).button(labels::ADD_CATEGORY.clone());
    assert_eq!(list.quick_reply, Some(QuickReply::new([add.clone()])));

    // Without any category, it says how to add one
    let s = setup().await;
    let empty = s.taps(s.admin, "categories").await;
    assert_eq!(empty.len(), 1);
    assert!(matches!(empty[0].body, Body::Text { .. }));
    assert_eq!(empty[0].quick_reply, Some(QuickReply::new([add])));
}

/// FR-LINE-11: a long list is paged, and an action shows the page its category is on.
#[tokio::test]
async fn a_long_list_is_paged() {
    let s = setup().await;
    let names: Vec<_> = (1..=CATEGORIES_PER_PAGE + 2)
        .map(|n| format!("หมวด {n}"))
        .collect();
    let names: Vec<_> = names.iter().map(String::as_str).collect();
    let added = s.add(&names).await;

    let first = s.taps(s.admin, "categories").await;
    let titles_first = titles(&first[0]);
    assert_eq!(titles_first.len(), CATEGORIES_PER_PAGE + 1);
    assert_eq!(titles_first.last().unwrap(), "", "then ดูเพิ่มเติม");
    let more = cards(&first[0]).last().unwrap().1[0].1.clone();
    assert_eq!(more, "categories?page=1");
    let second = s.taps(s.admin, &more).await;
    assert_eq!(
        titles(&second[0]),
        [
            format!("{}. หมวด {}", names.len() - 1, names.len() - 1),
            format!("{}. หมวด {}", names.len(), names.len()),
        ]
    );
    // Deactivating the last shows its page
    let last = added.last().unwrap();
    let shown = s
        .taps(s.admin, &format!("{DEACTIVATE}?c={}", last.id))
        .await;
    assert_eq!(titles(&shown[1]), titles(&second[0]));
    // A page past the last, from an old button, shows the first
    let past = s.taps(s.admin, "categories?page=9").await;
    assert_eq!(titles(&past[0]), titles_first);
}

/// FR-ADM-05, FR-ADM-08: a category is added last under a name checked as it is typed, and
/// renamed likewise; each change is audited.
#[tokio::test]
async fn categories_are_added_and_renamed_under_checked_names() {
    let s = setup().await;
    let t = &s.templates;
    let [food, sweets] = s.add(&["อาหาร", "ขนม"]).await.try_into().unwrap();

    let ask = s.taps(s.admin, ADD).await;
    assert_eq!(ask, [s.prompt(t.ask_category_name(NAME_CHARS, None))]);
    let too_long = "ข".repeat(NAME_CHARS + 1);
    assert_eq!(
        s.types(&too_long).await,
        [s.prompt(t.text_too_long(NAME_CHARS))]
    );
    assert_eq!(
        s.types("  ").await,
        [s.prompt(t.ask_category_name(NAME_CHARS, None))]
    );
    assert_eq!(
        s.types(" ขนม ").await,
        [s.prompt(t.category_name_taken("ขนม"))]
    );
    assert!(s.naming().await);
    let added = s.types(" ของ   ฝาก ").await;
    assert!(!s.naming().await, "the flow is over");
    assert_eq!(
        s.categories().await,
        [
            ("อาหาร".to_owned(), 1, true),
            ("ขนม".to_owned(), 2, true),
            ("ของ ฝาก".to_owned(), 3, true),
        ]
    );
    let gifts = categories::ordered().all(&s.db).await.unwrap()[2].id;
    assert_eq!(
        added,
        [t.category_added("ของ ฝาก"), s.list(Some(gifts)).await]
    );

    let ask = s.taps(s.admin, &format!("{RENAME}?c={}", sweets.id)).await;
    assert_eq!(
        ask,
        [s.prompt(t.ask_category_name(NAME_CHARS, Some("ขนม")))]
    );
    assert_eq!(
        s.types("อาหาร").await,
        [s.prompt(t.category_name_taken("อาหาร"))]
    );
    // The same name changes nothing
    assert_eq!(s.types("ขนม").await, [s.list(Some(sweets.id)).await]);
    assert!(!s.naming().await);
    s.taps(s.admin, &format!("{RENAME}?c={}", sweets.id)).await;
    let renamed = s.types("ขนม/ของหวาน").await;
    assert_eq!(
        renamed,
        [
            t.category_renamed("ขนม", "ขนม/ของหวาน"),
            s.list(Some(sweets.id)).await
        ]
    );
    assert_eq!(s.active().await, ["อาหาร", "ขนม/ของหวาน", "ของ ฝาก"]);

    // Renaming a category deleted meanwhile ends the flow
    s.taps(s.admin, &format!("{RENAME}?c={}", food.id)).await;
    category::Entity::delete_by_id(food.id)
        .exec(&s.db)
        .await
        .unwrap();
    assert_eq!(
        s.types("อาหารคาว").await,
        [t.denied(Denied::NotFound), s.list(None).await]
    );
    assert!(!s.naming().await);

    assert_eq!(
        s.audited().await,
        [
            s.by_admin("category.added", gifts, json!({ "name": "ของ ฝาก" })),
            s.by_admin(
                "category.renamed",
                sweets.id,
                json!({ "from": "ขนม", "to": "ขนม/ของหวาน" })
            ),
        ]
    );
}

/// AC (FR-ADM-05): moves change the order buyers see; a tap on a button sent before the order
/// changed, the same button tapped twice included, changes nothing. Each move is audited.
#[tokio::test]
async fn moves_reorder_and_a_stale_tap_changes_nothing() {
    let s = setup().await;
    let [_, sweets, eggs] = s.add(&["อาหาร", "ขนม", "ไข่/นม"]).await.try_into().unwrap();
    let list = s.taps(s.admin, "categories").await;
    let up = button(&list[0], "2. ขนม", &labels::MOVE_UP);

    assert_eq!(s.taps(s.admin, &up).await, [s.list(Some(sweets.id)).await]);
    assert_eq!(s.active().await, ["ขนม", "อาหาร", "ไข่/นม"]);
    // Again: อาหาร is no longer above ขนม
    assert_eq!(s.taps(s.admin, &up).await, [s.list(Some(sweets.id)).await]);
    assert_eq!(s.active().await, ["ขนม", "อาหาร", "ไข่/นม"]);
    let list = s.taps(s.admin, "categories").await;
    let down = button(&list[0], "1. ขนม", &labels::MOVE_DOWN);
    s.taps(s.admin, &down).await;
    s.taps(s.admin, &down).await;
    let down = button(&s.list(None).await, "2. ขนม", &labels::MOVE_DOWN);
    s.taps(s.admin, &down).await;
    assert_eq!(
        s.categories().await,
        [
            ("อาหาร".to_owned(), 1, true),
            ("ไข่/นม".to_owned(), 2, true),
            ("ขนม".to_owned(), 3, true),
        ]
    );
    // The last can't go further down, nor a gone category anywhere
    let gone = format!("{UP}?c=999&n={}", eggs.id);
    let t = &s.templates;
    assert_eq!(
        s.taps(s.admin, &gone).await,
        [t.denied(Denied::NotFound), s.list(None).await]
    );

    let moved = |from: i64, to: i64| {
        let details = json!({ "name": "ขนม", "from": from, "to": to });
        s.by_admin("category.moved", sweets.id, details)
    };
    assert_eq!(s.audited().await, [moved(2, 1), moved(1, 2), moved(2, 3)]);
}

/// AC (FR-ADM-05): a deactivated category is no longer offered for browsing or listing, until
/// activated again. Each change is audited; a repeated tap changes nothing.
#[tokio::test]
async fn deactivating_hides_a_category_from_browsing_and_listing() {
    let s = setup().await;
    let t = &s.templates;
    let [_, sweets, _] = s.add(&["อาหาร", "ขนม", "ไข่/นม"]).await.try_into().unwrap();
    s.list_product(sweets.id, "ขนมครก", product::Status::Active)
        .await;
    let deactivate = format!("{DEACTIVATE}?c={}", sweets.id);
    let activate = format!("{ACTIVATE}?c={}", sweets.id);

    let done = s.taps(s.admin, &deactivate).await;
    assert_eq!(
        done,
        [t.category_deactivated("ขนม"), s.list(Some(sweets.id)).await]
    );
    assert_eq!(s.active().await, ["อาหาร", "ไข่/นม"]);
    let browse = s
        .at(s.buyer, async |turn| {
            crate::catalog::browse::tapped(turn, &Item::Buy.postback()).await
        })
        .await;
    let items = &browse[0].quick_reply.as_ref().unwrap().items;
    let offered: Vec<_> = items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect();
    assert_eq!(offered, ["อาหาร", "ไข่/นม"]);
    // The products stay as they are
    let products = product::Entity::find().all(&s.db).await.unwrap();
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].status, product::Status::Active);

    assert_eq!(
        s.taps(s.admin, &deactivate).await,
        [s.list(Some(sweets.id)).await]
    );
    assert_eq!(
        s.taps(s.admin, &activate).await,
        [t.category_activated("ขนม"), s.list(Some(sweets.id)).await]
    );
    assert_eq!(s.active().await, ["อาหาร", "ขนม", "ไข่/นม"]);
    assert_eq!(
        s.audited().await,
        [
            s.by_admin("category.deactivated", sweets.id, json!({ "name": "ขนม" })),
            s.by_admin("category.activated", sweets.id, json!({ "name": "ขนม" })),
        ]
    );
}

/// AC (FR-ADM-05): a category that has or had products can't be deleted; deactivating it is
/// offered instead while it is active.
#[tokio::test]
async fn a_category_with_products_is_never_deleted() {
    let s = setup().await;
    let t = &s.templates;
    let [sweets, drinks] = s.add(&["ขนม", "เครื่องดื่ม"]).await.try_into().unwrap();
    s.list_product(sweets.id, "ขนมครก", product::Status::Active)
        .await;
    // Only a deleted product, which keeps its category (DR-04)
    s.list_product(drinks.id, "ชาเย็น", product::Status::Deleted)
        .await;

    let refused = s.taps(s.admin, &format!("{DELETE}?c={}", sweets.id)).await;
    let deactivate = Postback::new(DEACTIVATE).with_int(CATEGORY, sweets.id);
    let deactivate = deactivate.button(labels::DEACTIVATE.clone());
    assert_eq!(refused, [t.category_in_use("ขนม", Some(deactivate))]);
    s.taps(s.admin, &format!("{DEACTIVATE}?c={}", drinks.id))
        .await;
    let refused = s.taps(s.admin, &format!("{DELETE}?c={}", drinks.id)).await;
    assert_eq!(refused, [t.category_in_use("เครื่องดื่ม", None)]);

    assert_eq!(
        s.categories().await,
        [
            ("ขนม".to_owned(), 1, true),
            ("เครื่องดื่ม".to_owned(), 2, false),
        ]
    );
    assert_eq!(pending_action::Entity::find().all(&s.db).await.unwrap(), []);
    assert_eq!(s.audited().await.len(), 1, "only the deactivation");
}

/// FR-ADM-05, FR-UI-13: a category without products is deleted once confirmed, and only once;
/// one that got a product meanwhile is kept.
#[tokio::test]
async fn an_unused_category_is_deleted_once_confirmed() {
    let s = setup().await;
    let t = &s.templates;
    let [gifts, crafts] = s.add(&["ของฝาก", "งานฝีมือ"]).await.try_into().unwrap();

    let ask = s.taps(s.admin, &format!("{DELETE}?c={}", gifts.id)).await;
    assert_eq!(ask.len(), 1);
    let confirm = quick_postback(&ask[0]);
    let id = Postback::parse(&confirm).unwrap().uuid("pa").unwrap();
    let button = Postback::new(pending::CONFIRM)
        .with_name("k", DeleteCategory::KIND)
        .with_uuid("pa", id)
        .button(labels::CONFIRM_DELETE.clone());
    assert_eq!(ask, [t.confirm_delete_category("ของฝาก", button)]);
    assert_eq!(s.categories().await.len(), 2, "not before it is confirmed");

    let deleted = s.taps(s.admin, &confirm).await;
    assert_eq!(deleted, [t.category_deleted("ของฝาก"), s.list(None).await]);
    assert_eq!(s.categories().await, [("งานฝีมือ".to_owned(), 2, true)]);
    // Again: already done, so the list as it is
    assert_eq!(s.taps(s.admin, &confirm).await, [s.list(None).await]);
    // Nor can a buyer, who isn't shown the list either
    assert_eq!(
        s.taps(s.buyer, &confirm).await,
        [t.denied(Denied::NotAllowed)]
    );

    // A product listed before the confirmation keeps the category
    let ask = s.taps(s.admin, &format!("{DELETE}?c={}", crafts.id)).await;
    s.list_product(crafts.id, "ตะกร้าสาน", product::Status::Active)
        .await;
    let kept = s.taps(s.admin, &quick_postback(&ask[0])).await;
    let deactivate = Postback::new(DEACTIVATE).with_int(CATEGORY, crafts.id);
    let deactivate = deactivate.button(labels::DEACTIVATE.clone());
    assert_eq!(kept, [t.category_in_use("งานฝีมือ", Some(deactivate))]);
    assert_eq!(s.categories().await, [("งานฝีมือ".to_owned(), 2, true)]);

    assert_eq!(
        s.audited().await,
        [s.by_admin("category.deleted", gifts.id, json!({ "name": "ของฝาก" }))]
    );
}
