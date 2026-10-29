//! "ช่วยเหลือ" (FR-HELP-01, FR-HELP-03, FR-AI-14): each role sees its own topics, each topic is
//! told for the user's roles and what is configured, and every answer fits LINE and offers a
//! next step. The `async` tests that touch the database each start a throwaway PostgreSQL, so
//! they need Docker.

use chrono::{TimeZone, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ActiveModelTrait, DatabaseConnection, TransactionTrait};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;

use super::*;
use crate::config::{self, PoliteParticle, SessionConfig};
use crate::line::flex::{Component, FlexContainer};
use crate::line::{Body, limits};
use crate::persistence::entity::{shop, user};
use crate::persistence::{orm, testing};
use crate::users::access::Actor;

static SESSION: SessionConfig = SessionConfig {
    timeout_minutes: 15,
    pending_action_minutes: 60,
};

const BUYER: Roles = Roles {
    seller: false,
    admin: false,
};
const SELLER: Roles = Roles {
    seller: true,
    admin: false,
};
const ADMIN: Roles = Roles {
    seller: false,
    admin: true,
};
const SELLER_ADMIN: Roles = Roles {
    seller: true,
    admin: true,
};
const ALL_ROLES: [Roles; 4] = [BUYER, SELLER, ADMIN, SELLER_ADMIN];

/// 24 ต.ค. 2569 14:30 in Bangkok
fn t0() -> DateTimeUtc {
    Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap()
}

/// The sample config, with a notice text in neither voice so either voice can be checked.
fn sample() -> Config {
    let mut config = config::sample();
    config.privacy.notice_text = "ตลาดนี้ดูแลโดยคณะกรรมการหมู่บ้าน".to_owned();
    config
}

/// The gateway for `config`; the AI needs no database to say which features are on.
fn gateway(config: &Config) -> Gateway {
    Gateway::new(&config.ai, chrono_tz::Asia::Bangkok, testing::unreachable())
}

fn templates(config: &Config) -> Templates {
    Templates::new(&config.app)
}

/// The article of `topic` for `roles` under `config`.
fn read(config: &Config, roles: Roles, topic: Topic) -> String {
    article(&templates(config), config, &gateway(config), roles, topic)
}

/// The label and postback data of each topic on the "ช่วยเหลือ" card.
fn topics(message: &Message) -> Vec<(String, String)> {
    let Body::Flex {
        contents: FlexContainer::Bubble(bubble),
        ..
    } = &message.body
    else {
        panic!("not a card: {message:?}");
    };
    let body = bubble.body.as_ref().expect("a body");
    body.contents
        .iter()
        .filter_map(|component| match component {
            Component::Button(button) => match &button.action {
                Action::Postback { label, data, .. } => {
                    Some((label.as_str().to_owned(), data.clone()))
                }
                action => panic!("not a postback: {action:?}"),
            },
            _ => None,
        })
        .collect()
}

fn text(message: &Message) -> &str {
    match &message.body {
        Body::Text { text } => text,
        body => panic!("not text: {body:?}"),
    }
}

/// The labels of `message`'s Quick Replies.
fn quick_replies(message: &Message) -> Vec<&str> {
    let quick_reply = message.quick_reply.as_ref().expect("a Quick Reply");
    quick_reply
        .items
        .iter()
        .map(|item| item.action.label().as_str())
        .collect()
}

fn labels_of(actions: &[Action]) -> Vec<&str> {
    actions
        .iter()
        .map(|action| action.label().as_str())
        .collect()
}

/// Acceptance: buyers, sellers and admins each see only the topics for their roles.
#[test]
fn each_role_sees_its_own_topics() {
    let everyone = [
        "วิธีสั่งซื้อ",
        "เปิดร้านและลงสินค้า",
        "การชำระเงิน",
        "ข้อมูลของฉัน",
        "ความเป็นส่วนตัว",
        "แจ้งปัญหา",
        "ติดต่อผู้ดูแล",
        "ลบบัญชี",
    ];
    let mut admins = everyone.to_vec();
    admins.push("งานผู้ดูแล");
    let t = templates(&sample());
    for (roles, expected) in [
        (BUYER, everyone.to_vec()),
        (SELLER, everyone.to_vec()),
        (ADMIN, admins.clone()),
        (SELLER_ADMIN, admins),
    ] {
        let card = t.help(Topic::for_roles(roles));
        let shown = topics(&card);
        let labels: Vec<_> = shown.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, expected, "{roles:?}");
        for (_, data) in &shown {
            let postback = Postback::parse(data).unwrap();
            let topic = postback.action().strip_prefix(PREFIX).unwrap();
            assert!(Topic::from_key(topic).unwrap().is_for(roles));
        }
        limits::check(&[card]).unwrap();
    }
    assert!(!Topic::Admin.is_for(SELLER));
}

#[test]
fn topics_are_named_by_their_keys() {
    for topic in Topic::ALL {
        assert_eq!(Topic::from_key(topic.key()), Some(topic));
        assert_eq!(format!("{PREFIX}{}", topic.key()), topic.action());
        assert!(!topic.key().contains('.'), "{topic:?}");
        let data = topic.postback().to_string();
        assert_eq!(Postback::parse(&data), Some(topic.postback()));
    }
    assert_eq!(Topic::from_key("help"), None);
    assert_eq!(Topic::from_key(""), None);
}

/// FR-UI-03, FR-UI-08, FR-LINE-11: every topic, for every role, in either voice, with the AI
/// on or off and either payment method, fits one message, says what to do next, and keeps to
/// the configured voice.
#[tokio::test]
async fn every_answer_fits_and_keeps_the_voice() {
    for particle in [PoliteParticle::Kha, PoliteParticle::Khrap] {
        for ai in [true, false] {
            for methods in [vec![PaymentMethod::Cash], vec![PaymentMethod::PromptPay]] {
                let mut config = sample();
                config.app.polite_particle = particle;
                config.payment.methods = methods;
                if !ai {
                    config.ai.primary = None;
                }
                let t = templates(&config);
                for roles in ALL_ROLES {
                    for has_shop in [false, true] {
                        for topic in Topic::for_roles(roles) {
                            let article = read(&config, roles, topic);
                            // คะแนน, the stars of "ให้คะแนน" (FR-ORD-14), is no particle
                            let voice = article.replace("คะแนน", "");
                            match particle {
                                PoliteParticle::Khrap => {
                                    assert!(article.contains("ครับ"), "{topic:?}: {article}");
                                    assert!(
                                        !voice.contains("ค่ะ") && !voice.contains("คะ"),
                                        "{topic:?}: {article}"
                                    );
                                }
                                PoliteParticle::Kha => {
                                    assert!(article.contains("ค่ะ"), "{topic:?}: {article}");
                                    assert!(!article.contains("ครับ"), "{topic:?}: {article}");
                                }
                            }
                            let next = next_steps(roles, has_shop, &config, topic);
                            let message = t.help_topic(article, next);
                            limits::check(std::slice::from_ref(&message)).unwrap();
                            // The way back is always there (FR-UI-03)
                            let replies = quick_replies(&message);
                            assert_eq!(replies[replies.len() - 2..], ["ช่วยเหลือ", "เมนู"]);
                        }
                    }
                }
            }
        }
    }
}

/// FR-HELP-01: a seller learns how to list and run the shop; anyone else how to open one.
#[tokio::test]
async fn the_shop_topic_is_told_for_the_role() {
    let config = sample();
    let opening = read(&config, BUYER, Topic::Selling);
    assert!(opening.starts_with("วิธีเปิดร้านและลงสินค้า"), "{opening}");
    assert!(opening.contains("\"เปิดร้าน\"") && opening.contains("รอผู้ดูแลอนุมัติ"));
    let mut no_approval = sample();
    no_approval.shop.require_approval = false;
    let opening = read(&no_approval, BUYER, Topic::Selling);
    assert!(opening.contains("ร้านเปิดได้ทันที") && !opening.contains("อนุมัติ"));

    let selling = read(&config, SELLER, Topic::Selling);
    assert!(selling.starts_with("วิธีลงสินค้าและดูแลร้าน"), "{selling}");
    for menu in [
        "\"เพิ่มสินค้า\"",
        "\"สินค้าของฉัน\"",
        "\"เปิด/ปิดร้าน\"",
        "\"ออเดอร์ร้าน\"",
    ] {
        assert!(selling.contains(menu), "{menu}: {selling}");
    }
    assert!(selling.contains("ภายใน 60 นาที"), "{selling}");
    // The AI drafts listings only while it is on (FR-PROD-02)
    assert!(selling.contains("ส่งรูปสินค้า"));
    let mut ai_off = sample();
    ai_off.ai.primary = None;
    assert!(!read(&ai_off, SELLER, Topic::Selling).contains("ส่งรูปสินค้า"));

    let next = |roles, has_shop| {
        let next = next_steps(roles, has_shop, &config, Topic::Selling);
        labels_of(&next).join(" · ")
    };
    assert_eq!(next(BUYER, false), "เปิดร้าน");
    // An application waiting for approval is the user's shop already
    assert_eq!(next(BUYER, true), "ร้านของฉัน");
    assert_eq!(next(SELLER, true), "เพิ่มสินค้า · สินค้าของฉัน · ออเดอร์ร้าน");
}

/// FR-PAY-02: the payment topic names only the methods configured, and tells sellers their
/// side too.
#[tokio::test]
async fn the_payment_topic_names_the_methods_configured() {
    let mut cash = sample();
    cash.payment.methods = vec![PaymentMethod::Cash];
    let buyer = read(&cash, BUYER, Topic::Payment);
    assert!(
        buyer.contains("เงินสด") && !buyer.contains("พร้อมเพย์"),
        "{buyer}"
    );
    assert!(!buyer.contains("สำหรับร้าน"));
    let seller = read(&cash, SELLER, Topic::Payment);
    assert!(seller.contains("\"ได้รับเงินแล้ว\"") && !seller.contains("ตั้งค่าร้าน"));

    let mut promptpay = sample();
    promptpay.payment.methods = vec![PaymentMethod::PromptPay];
    let buyer = read(&promptpay, BUYER, Topic::Payment);
    assert!(
        buyer.contains("โอนพร้อมเพย์") && !buyer.contains("เงินสด"),
        "{buyer}"
    );
    let seller = read(&promptpay, SELLER, Topic::Payment);
    assert!(seller.contains("\"ตั้งค่าร้าน\"") && seller.contains("\"คืนเงินแล้ว\""));

    let next = next_steps(SELLER, true, &promptpay, Topic::Payment);
    assert_eq!(labels_of(&next), ["คำสั่งซื้อของฉัน", "ออเดอร์ร้าน"]);
}

/// FR-HELP-03: "ติดต่อผู้ดูแล" shows `village.adminContact`, on a line of its own.
#[tokio::test]
async fn contact_shows_the_configured_text() {
    let mut config = sample();
    config.village.admin_contact = Some("ลุงชัย ประธานหมู่บ้าน โทร 081-234-5678".to_owned());
    let contact = read(&config, BUYER, Topic::Contact);
    assert_eq!(
        contact.lines().collect::<Vec<_>>(),
        ["ติดต่อผู้ดูแลได้ที่นี่ค่ะ", "ลุงชัย ประธานหมู่บ้าน โทร 081-234-5678"]
    );
    config.village.admin_contact = None;
    let contact = read(&config, BUYER, Topic::Contact);
    assert!(contact.starts_with("ติดต่อผู้ดูแลหมู่บ้านได้โดยตรง"), "{contact}");
    assert!(contact.contains("\"แจ้งปัญหา\""));
}

/// NFR-PRIV-02: "ความเป็นส่วนตัว" says what the privacy notice says.
#[tokio::test]
async fn the_privacy_topic_says_what_the_notice_says() {
    let mut config = sample();
    config.privacy.notice_url = Some("https://market.example.com/privacy".parse().unwrap());
    let privacy = read(&config, BUYER, Topic::Privacy);
    let facts = onboarding::privacy_facts(&config);
    assert!(privacy.contains(facts.text));
    for line in crate::notifications::templates::privacy_lines(&facts) {
        assert!(privacy.contains(&line), "{line}");
    }
    assert!(privacy.contains("Anthropic (Claude)"));
    assert!(privacy.contains("ฉบับที่ 1") && privacy.contains("\"อ่านฉบับเต็ม\""));
    let next = next_steps(BUYER, false, &config, Topic::Privacy);
    assert_eq!(
        next,
        [Action::Uri {
            label: labels::FULL_NOTICE.clone(),
            uri: "https://market.example.com/privacy".to_owned(),
        }]
    );
    config.privacy.notice_url = None;
    assert!(!read(&config, BUYER, Topic::Privacy).contains("อ่านฉบับเต็ม"));
    assert_eq!(next_steps(BUYER, false, &config, Topic::Privacy), []);
}

/// The topics that lead to a function offer it: "ข้อมูลของฉัน", "ลบบัญชี" (#36), and from
/// "แจ้งปัญหา" the admins' contact.
#[tokio::test]
async fn topics_lead_to_their_functions() {
    let config = sample();
    let data = |topic| {
        next_steps(BUYER, false, &config, topic)
            .into_iter()
            .map(|action| match action {
                Action::Postback { data, .. } => data,
                action => panic!("not a postback: {action:?}"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(data(Topic::MyData), [profile::SHOW]);
    assert_eq!(data(Topic::DeleteAccount), [DELETE_ACCOUNT]);
    assert_eq!(data(Topic::Report), ["help.contact"]);
    assert_eq!(data(Topic::Ordering), ["buy", "search", "cart"]);
    assert_eq!(data(Topic::Contact), Vec::<String>::new());
    let admin = next_steps(ADMIN, false, &config, Topic::Admin);
    assert_eq!(labels_of(&admin), ["รายการรอตรวจ", "อนุมัติร้าน"]);

    let report = read(&config, BUYER, Topic::Report);
    assert!(report.contains("กด \"แจ้งปัญหา\" ที่สินค้าหรือร้านนั้น"), "{report}");
    let delete = read(&config, BUYER, Topic::DeleteAccount);
    assert!(delete.contains("ออเดอร์ที่ยังไม่จบ") && delete.contains("กู้คืนไม่ได้"));
}

/// FR-AI-01: the assistant is mentioned only while it is on, and the admins' alerts only while
/// `notification.adminAlerts` is.
#[tokio::test]
async fn articles_mention_only_what_is_on() {
    let config = sample();
    assert!(read(&config, BUYER, Topic::Ordering).contains("ผู้ช่วยจะหาให้"));
    assert!(read(&config, BUYER, Topic::Ordering).contains("ทุกครั้งที่สถานะเปลี่ยน"));
    assert!(read(&config, ADMIN, Topic::Admin).contains("ระบบจะแจ้งเตือน"));
    let mut off = sample();
    off.ai.primary = None;
    off.notification.order_status_to_buyer = false;
    off.notification.admin_alerts = false;
    let ordering = read(&off, BUYER, Topic::Ordering);
    assert!(!ordering.contains("ผู้ช่วย") && !ordering.contains("สถานะเปลี่ยน"));
    assert!(!read(&off, ADMIN, Topic::Admin).contains("ระบบจะแจ้งเตือน"));
}

struct Setup {
    _container: ContainerAsync<Postgres>,
    db: DatabaseConnection,
    config: Config,
    templates: Templates,
}

async fn setup() -> Setup {
    let (container, pool) = testing::migrated().await;
    let config = sample();
    Setup {
        _container: container,
        db: orm(pool),
        templates: templates(&config),
        config,
    }
}

impl Setup {
    /// User `user_id` taps a button sending `data`, in a transaction committed once handled.
    async fn taps(&self, user_id: i64, data: &str) -> Vec<Message> {
        let postback = Postback::parse(data).unwrap();
        let ai = gateway(&self.config);
        let txn = self.db.begin().await.unwrap();
        let actor = Actor::load(&txn, user_id).await.unwrap().expect("a user");
        let turn = Turn::new(&txn, &self.templates, &SESSION, actor, t0())
            .await
            .unwrap();
        let reply = tapped(&turn, &self.config, &ai, &postback).await.unwrap();
        drop(turn);
        txn.commit().await.unwrap();
        reply
    }

    async fn user(&self, line_user_id: &str, admin: bool, status: user::Status) -> i64 {
        let user = user::ActiveModel {
            is_admin: Set(admin),
            status: Set(status),
            ..testing::new_user(line_user_id)
        };
        user.insert(&self.db).await.unwrap().id
    }
}

/// FR-HELP-01, FR-ADM-04: help as each user sees it: their roles' topics, a topic of a role
/// they don't hold showing the topics instead, and help still open to suspended users.
#[tokio::test]
async fn help_is_answered_for_the_users_roles() {
    let s = setup().await;
    let buyer = s.user("U-buyer", false, user::Status::Active).await;
    let admin = s.user("U-admin", true, user::Status::Active).await;
    let suspended = s.user("U-suspended", false, user::Status::Suspended).await;
    let seller = s.user("U-seller", false, user::Status::Active).await;
    testing::new_shop(seller).insert(&s.db).await.unwrap();
    let pending = s.user("U-pending", false, user::Status::Active).await;
    let application = shop::ActiveModel {
        status: Set(shop::Status::Pending),
        ..testing::new_shop(pending)
    };
    application.insert(&s.db).await.unwrap();

    let t = &s.templates;
    let card = |roles| vec![t.help(Topic::for_roles(roles))];
    assert_eq!(s.taps(buyer, "help").await, card(BUYER));
    assert_eq!(s.taps(admin, "help").await, card(ADMIN));
    assert_eq!(s.taps(seller, "help").await, card(SELLER));
    // A pending application gives no seller role yet
    assert_eq!(s.taps(pending, "help").await, card(BUYER));
    assert_eq!(s.taps(suspended, "help").await, card(BUYER));

    // A topic of a role the user doesn't hold, or none at all, shows the topics
    for data in ["help.admin", "help.nothing"] {
        assert_eq!(s.taps(buyer, data).await, card(BUYER), "{data}");
    }

    let answer = |roles, has_shop, topic| {
        let article = read(&s.config, roles, topic);
        vec![t.help_topic(article, next_steps(roles, has_shop, &s.config, topic))]
    };
    assert_eq!(
        s.taps(admin, "help.admin").await,
        answer(ADMIN, false, Topic::Admin)
    );
    assert_eq!(
        s.taps(seller, "help.selling").await,
        answer(SELLER, true, Topic::Selling)
    );
    assert_eq!(
        s.taps(pending, "help.selling").await,
        answer(BUYER, true, Topic::Selling)
    );
    let contact = s.taps(suspended, "help.contact").await;
    assert_eq!(contact, answer(BUYER, false, Topic::Contact));
    assert!(text(&contact[0]).contains("ติดต่อผู้ดูแล"));
}
