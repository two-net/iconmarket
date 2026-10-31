//! The client against the fake LINE of [`testing`](super::testing), which records every
//! request and answers from a script. Pushes follow LINE's retry-key rule, so the tests can
//! count deliveries.

use reqwest::StatusCode;
use serde_json::json;
use url::Url;

use super::flex::{
    Bubble, BubbleSize, Button, ButtonStyle, Carousel, FlexBox, Image, Size as TextSize, Text,
    Weight,
};
use super::limits;
use super::testing::{
    Answer, BOT_INFO, Fake, GROUP, PUSH, QUOTA, QUOTA_CONSUMPTION, REPLY, TOKEN, USER,
};
use super::*;
use crate::config::Secret;
use crate::thai::Label;

fn messages() -> Vec<Message> {
    vec![Message::text("ไข่ไก่มาส่งแล้วค่ะ")]
}

fn label(text: &'static str) -> Label {
    Label::fit(text)
}

// Push (FR-LINE-12)

/// AC: a push that gets a 500 and then succeeds is delivered exactly once, with the same
/// retry key.
#[tokio::test]
async fn a_push_retried_after_a_server_error_is_delivered_once_with_the_same_key() {
    let fake = Fake::start().await;
    fake.script(PUSH, [Answer::error(500, "Internal server error")]);
    let key = RetryKey::random();

    let pushed = fake.client().push(USER, &messages(), key).await.unwrap();

    assert_eq!(pushed.attempts, 2);
    assert_eq!(pushed.request_id.as_deref(), Some("req-2"));
    assert_eq!(fake.deliveries(), 1);
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!((request.host, request.path.as_str()), ("api", PUSH));
        assert_eq!(
            request.header("x-line-retry-key"),
            Some(&*key.uuid().to_string())
        );
        assert_eq!(request.json(), requests[0].json());
    }
}

#[tokio::test]
async fn a_push_whose_answer_was_lost_is_not_delivered_again() {
    let fake = Fake::start().await;
    fake.script(
        PUSH,
        [Answer::error(500, "Internal server error").after_delivering()],
    );

    let pushed = fake
        .client()
        .push(USER, &messages(), RetryKey::random())
        .await
        .unwrap();

    // LINE answered the retry with 409 and the ID of the request that delivered it
    assert_eq!(pushed.attempts, 2);
    assert_eq!(pushed.request_id.as_deref(), Some("req-1"));
    assert_eq!(fake.deliveries(), 1);
}

#[tokio::test]
async fn a_push_that_timed_out_is_retried_with_the_same_key() {
    let fake = Fake::start().await;
    fake.script(PUSH, [Answer::ok().slow()]);

    let pushed = fake
        .client()
        .push(USER, &messages(), RetryKey::random())
        .await
        .unwrap();

    assert_eq!(pushed.attempts, 2);
    assert_eq!(fake.deliveries(), 1);
    let requests = fake.requests();
    assert_eq!(
        requests[0].header("x-line-retry-key"),
        requests[1].header("x-line-retry-key")
    );
}

/// AC: a monthly-quota error is not retried and is reported to the caller.
#[tokio::test]
async fn the_monthly_quota_is_reported_and_not_retried() {
    let fake = Fake::start().await;
    fake.script(
        PUSH,
        [Answer::error(429, "You have reached your monthly limit.")],
    );

    let error = fake
        .client()
        .push(USER, &messages(), RetryKey::random())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, Error::Quota { request_id } if request_id.as_deref() == Some("req-1")),
        "{error:?}"
    );
    assert!(!error.is_retryable());
    assert_eq!(fake.requests().len(), 1);
    assert_eq!(fake.deliveries(), 0);
}

#[tokio::test]
async fn the_rate_limit_is_retried() {
    let fake = Fake::start().await;
    let rate_limited = Answer::error(
        429,
        "The API rate limit has been exceeded. Try again later.",
    );
    fake.script(PUSH, [rate_limited.clone(), rate_limited]);

    let pushed = fake
        .client()
        .push(USER, &messages(), RetryKey::random())
        .await
        .unwrap();

    assert_eq!(pushed.attempts, 3);
    assert_eq!(fake.deliveries(), 1);
}

#[tokio::test]
async fn a_push_is_retried_at_most_three_times() {
    let fake = Fake::start().await;
    fake.script(PUSH, vec![Answer::error(503, "Service unavailable"); 5]);

    let error = fake
        .client()
        .push(USER, &messages(), RetryKey::random())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, Error::Api { status, .. } if *status == StatusCode::SERVICE_UNAVAILABLE),
        "{error:?}"
    );
    assert_eq!(fake.requests().len(), 4);
    assert_eq!(fake.deliveries(), 0);
}

#[tokio::test]
async fn other_refusals_are_not_retried() {
    let fake = Fake::start().await;
    fake.script(PUSH, [Answer::error(400, "Failed to send messages")]);

    let error = fake
        .client()
        .push(USER, &messages(), RetryKey::random())
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "LINE answered 400 Bad Request: Failed to send messages [request req-1]"
    );
    assert_eq!(fake.requests().len(), 1);
}

/// A queued notification is sent as stored, and retried like any push.
#[tokio::test]
async fn stored_messages_are_pushed_as_they_are_with_the_same_retries() {
    let fake = Fake::start().await;
    fake.script(PUSH, [Answer::error(500, "Internal server error")]);
    let stored = serde_json::to_value(messages()).unwrap();
    let key = RetryKey::random();

    let pushed = fake.client().push_stored(USER, &stored, key).await.unwrap();

    assert_eq!(pushed.attempts, 2);
    assert_eq!(fake.deliveries(), 1);
    let requests = fake.requests();
    assert_eq!(
        requests[1].json(),
        json!({ "to": USER, "messages": [{ "type": "text", "text": "ไข่ไก่มาส่งแล้วค่ะ" }] })
    );
    assert_eq!(
        requests[1].header("x-line-retry-key"),
        Some(&*key.uuid().to_string())
    );
}

#[test]
fn retry_keys_are_version_4_uuids() {
    let key = RetryKey::random().uuid();
    assert_eq!(key.get_version_num(), 4);
    // The RFC 4122 variant
    assert_eq!(key.as_bytes()[8] & 0xc0, 0x80);
    assert_ne!(RetryKey::random(), RetryKey::random());
}

// Reply and respond (FR-LINE-07)

#[tokio::test]
async fn a_reply_sends_the_token_and_messages_and_no_retry_key() {
    let fake = Fake::start().await;

    fake.client()
        .reply("reply-token", &messages())
        .await
        .unwrap();

    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", REPLY)
    );
    assert_eq!(
        request.json(),
        json!({
            "replyToken": "reply-token",
            "messages": [{ "type": "text", "text": "ไข่ไก่มาส่งแล้วค่ะ" }],
        })
    );
    assert_eq!(
        request.header("authorization"),
        Some(&*format!("Bearer {TOKEN}"))
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    // LINE refuses a retry key on a reply
    assert_eq!(request.header("x-line-retry-key"), None);
    assert!(
        request
            .header("user-agent")
            .unwrap()
            .starts_with("icon-market/")
    );
}

#[tokio::test]
async fn a_failed_reply_is_not_retried() {
    let fake = Fake::start().await;
    fake.script(REPLY, [Answer::error(500, "Internal server error")]);

    let error = fake
        .client()
        .reply("reply-token", &messages())
        .await
        .unwrap_err();

    assert!(error.is_retryable(), "a push would be retried");
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn a_response_goes_by_reply_when_it_can() {
    let fake = Fake::start().await;

    let delivery = fake
        .client()
        .respond(
            Some("reply-token"),
            USER,
            &messages(),
            Importance::Essential,
        )
        .await
        .unwrap();

    assert!(matches!(delivery, Delivery::Replied), "{delivery:?}");
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn an_essential_response_is_pushed_when_the_reply_fails() {
    let fake = Fake::start().await;
    fake.script(REPLY, [Answer::error(400, "Invalid reply token")]);

    let delivery = fake
        .client()
        .respond(
            Some("expired-token"),
            USER,
            &messages(),
            Importance::Essential,
        )
        .await
        .unwrap();

    let Delivery::Pushed { retry_key, result } = delivery else {
        panic!("not pushed: {delivery:?}");
    };
    assert_eq!(result.unwrap().attempts, 1);
    let requests = fake.requests();
    let paths: Vec<_> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, [REPLY, PUSH]);
    let push = &requests[1];
    assert_eq!(
        push.header("x-line-retry-key"),
        Some(&*retry_key.uuid().to_string())
    );
    assert_eq!(push.json()["to"], USER);
    assert_eq!(push.json()["messages"], requests[0].json()["messages"]);
    assert_eq!(fake.deliveries(), 1);
}

#[tokio::test]
async fn an_essential_response_without_a_reply_token_is_pushed() {
    let fake = Fake::start().await;
    fake.script(PUSH, [Answer::error(400, "Failed to send messages")]);

    let delivery = fake
        .client()
        .respond(None, USER, &messages(), Importance::Essential)
        .await
        .unwrap();

    // A failed push still comes back with its key, for the push log
    let Delivery::Pushed { result, .. } = delivery else {
        panic!("not pushed: {delivery:?}");
    };
    assert!(matches!(result, Err(Error::Api { .. })), "{result:?}");
    let paths: Vec<_> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [PUSH]);
}

#[tokio::test]
async fn an_optional_response_is_dropped_when_the_reply_fails() {
    let fake = Fake::start().await;
    fake.script(REPLY, [Answer::error(400, "Invalid reply token")]);
    let client = fake.client();

    let failed = client
        .respond(
            Some("expired-token"),
            USER,
            &messages(),
            Importance::Optional,
        )
        .await
        .unwrap();
    let no_token = client
        .respond(None, USER, &messages(), Importance::Optional)
        .await
        .unwrap();

    assert!(matches!(failed, Delivery::Dropped), "{failed:?}");
    assert!(matches!(no_token, Delivery::Dropped), "{no_token:?}");
    let paths: Vec<_> = fake.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [REPLY]);
}

// Limits (FR-LINE-11)

fn https(path: &str) -> Url {
    Url::parse("https://market.example.com/img/p/")
        .unwrap()
        .join(path)
        .unwrap()
}

fn postback(data: &str) -> Action {
    Action::postback(label("ดู"), data)
}

fn bubble_with(action: Action) -> Bubble {
    Bubble {
        body: Some(FlexBox::vertical([Button::new(action).into()])),
        ..Bubble::default()
    }
}

#[tokio::test]
async fn messages_over_a_limit_are_never_sent() {
    let fake = Fake::start().await;
    let client = fake.client();
    let text = Message::text("ค่ะ");
    let card = Bubble {
        body: Some(FlexBox::vertical([Text::new("ไข่ไก่").into()])),
        ..Bubble::default()
    };
    let cases: Vec<(&str, Vec<Message>)> = vec![
        ("messages", vec![]),
        ("messages", vec![text.clone(); 6]),
        ("messages[0].text", vec![Message::text("ก".repeat(5001))]),
        ("messages[0].text", vec![Message::text("")]),
        (
            "messages[0].quickReply.items",
            vec![
                text.clone()
                    .with_quick_reply(QuickReply::new(vec![postback("a"); 14])),
            ],
        ),
        (
            "messages[0].quickReply.items[0].action.data",
            vec![
                text.clone()
                    .with_quick_reply(QuickReply::new([postback(&"a".repeat(301))])),
            ],
        ),
        (
            "messages[0].quickReply.items[0].action.label",
            vec![
                text.clone()
                    .with_quick_reply(QuickReply::new([Action::postback(Label::fit(""), "a")])),
            ],
        ),
        (
            "messages[0].contents.contents",
            vec![Message::flex("สินค้า", Carousel::new(vec![card.clone(); 13]))],
        ),
        (
            "messages[0].contents.contents",
            vec![Message::flex(
                "สินค้า",
                Carousel::new(vec![
                    card.clone(),
                    Bubble {
                        size: Some(BubbleSize::Kilo),
                        ..card.clone()
                    },
                ]),
            )],
        ),
        (
            "messages[0].contents.body.contents[0].action.data",
            vec![Message::flex(
                "สินค้า",
                bubble_with(postback(&"a".repeat(301))),
            )],
        ),
        (
            "messages[0].contents.body.contents[0].action",
            vec![Message::flex(
                "สินค้า",
                bubble_with(Action::Location {
                    label: label("ส่งตำแหน่ง"),
                }),
            )],
        ),
        (
            "messages[0].quickReply.items[0].action",
            vec![
                text.clone()
                    .with_quick_reply(QuickReply::new([Action::RichMenuSwitch {
                        label: label("ผู้ขาย"),
                        rich_menu_alias_id: "seller".to_owned(),
                        data: "menu:seller".to_owned(),
                    }])),
            ],
        ),
        (
            "messages[0].originalContentUrl",
            vec![Message::image(
                &Url::parse("http://market.example.com/img/p/1").unwrap(),
                &https("1"),
            )],
        ),
        (
            "messages[0].contents",
            vec![Message::flex(
                "สินค้า",
                Bubble {
                    body: Some(FlexBox::vertical(
                        (0..400).map(|_| Text::new("ไข่ไก่สดจากฟาร์ม ".repeat(5)).into()),
                    )),
                    ..Bubble::default()
                },
            )],
        ),
    ];

    for (path, messages) in cases {
        let reply = client.reply("reply-token", &messages).await;
        let push = client.push(USER, &messages, RetryKey::random()).await;
        for result in [reply.map(drop), push.map(drop)] {
            match result {
                Err(Error::Limit(e)) => assert_eq!(e.path, path, "{e}"),
                other => panic!("{path}: {other:?}"),
            }
        }
    }
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn a_response_over_a_limit_is_neither_replied_nor_pushed() {
    let fake = Fake::start().await;
    let six = vec![Message::text("ค่ะ"); 6];

    let result = fake
        .client()
        .respond(Some("reply-token"), USER, &six, Importance::Essential)
        .await;

    assert!(result.is_err());
    assert!(fake.requests().is_empty());
}

#[test]
fn messages_at_every_limit_pass() {
    let full_card = bubble_with(Action::Postback {
        label: Label::fit("ก".repeat(20)),
        data: "a".repeat(300),
        display_text: Some("ข".repeat(300)),
    });
    let messages = vec![
        Message::text("ก".repeat(5000)),
        Message::flex("ก".repeat(5000), Carousel::new(vec![full_card; 12])),
        Message::image(&https("1"), &https("1?preview")),
        Message::text("ค่ะ").with_quick_reply(QuickReply::new(vec![postback("a"); 13])),
        Message::text("ค่ะ").with_quick_reply(QuickReply::new([
            Action::Camera {
                label: label("ถ่ายรูป"),
            },
            Action::CameraRoll {
                label: label("เลือกรูป"),
            },
            Action::Location {
                label: label("ส่งตำแหน่ง"),
            },
            Action::message(label("เมนู"), "เมนู"),
            Action::Uri {
                label: label("โทร"),
                uri: "tel:0812345678".to_owned(),
            },
        ])),
    ];
    limits::check(&messages).unwrap();
}

#[test]
fn long_alt_texts_are_shortened() {
    let message = Message::flex("ก".repeat(2000), bubble_with(postback("a")));
    let Body::Flex { alt_text, .. } = &message.body else {
        panic!("not a flex message");
    };
    assert_eq!(alt_text.chars().count(), limits::ALT_TEXT_CHARS);
    assert!(alt_text.ends_with('…'));
}

#[test]
fn pages_split_a_list_in_order() {
    let items: Vec<u32> = (1..=25).collect();
    let first = limits::page(&items, 0, 10);
    assert_eq!((first.items, first.next), (&items[..10], Some(1)));
    let last = limits::page(&items, 2, 10);
    assert_eq!((last.items, last.next), (&items[20..], None));
    let beyond = limits::page(&items, 9, 10);
    assert_eq!((beyond.items, beyond.next), (&[][..], None));
    let exact = limits::page(&items[..20], 1, 10);
    assert_eq!(exact.next, None);
}

// Loading, content and profiles (FR-LINE-08 – FR-LINE-10)

#[tokio::test]
async fn the_loading_animation_is_shown_in_one_to_one_chats_only() {
    let fake = Fake::start().await;
    let client = fake.client();

    client.start_loading(USER, 23).await.unwrap();
    client.start_loading(GROUP, 23).await.unwrap();

    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v2/bot/chat/loading/start");
    assert_eq!(
        requests[0].json(),
        json!({ "chatId": USER, "loadingSeconds": 25 })
    );
}

#[tokio::test]
async fn content_is_downloaded_from_the_content_host() {
    let fake = Fake::start().await;
    let path = "/v2/bot/message/5001/content";
    fake.script(path, [Answer::bytes("image/jpeg", b"\xff\xd8jpeg")]);

    let content = fake.client().content("5001", 1024).await.unwrap();

    assert_eq!(content.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(content.bytes, b"\xff\xd8jpeg");
    let requests = fake.requests();
    assert_eq!(
        (requests[0].host, requests[0].path.as_str()),
        ("data", path)
    );
}

#[tokio::test]
async fn content_still_being_prepared_is_asked_for_again() {
    let fake = Fake::start().await;
    let path = "/v2/bot/message/5002/content";
    let preparing = Answer::json(202, json!({}));
    fake.script(
        path,
        [
            preparing.clone(),
            preparing.clone(),
            Answer::bytes("audio/m4a", b"m4a"),
        ],
    );
    let client = fake.client();

    let content = client.content("5002", 1024).await.unwrap();
    assert_eq!(content.bytes, b"m4a");
    assert_eq!(fake.requests().len(), 3);

    // Not ready after ten more tries
    fake.script("/v2/bot/message/5003/content", vec![preparing; 20]);
    let error = client.content("5003", 1024).await.unwrap_err();
    assert!(matches!(error, Error::NotReady), "{error:?}");
    assert_eq!(fake.requests().len(), 3 + 11);
}

#[tokio::test]
async fn content_over_the_cap_is_refused() {
    let fake = Fake::start().await;
    fake.script(
        "/v2/bot/message/5004/content",
        [Answer::bytes("image/jpeg", &[0; 2048])],
    );

    let error = fake.client().content("5004", 1024).await.unwrap_err();

    assert!(
        matches!(error, Error::TooLarge { max_bytes: 1024 }),
        "{error:?}"
    );
}

#[tokio::test]
async fn unsent_content_is_gone() {
    let fake = Fake::start().await;
    fake.script(
        "/v2/bot/message/5005/content",
        [Answer::error(410, "The content is gone")],
    );

    let error = fake.client().content("5005", 1024).await.unwrap_err();

    assert!(
        matches!(&error, Error::Api { status, .. } if *status == StatusCode::GONE),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_profile_has_the_name_and_picture() {
    let fake = Fake::start().await;
    fake.script(
        &format!("/v2/bot/profile/{USER}"),
        [Answer::json(
            200,
            json!({
                "displayName": "ป้าแดง 🥚",
                "userId": USER,
                "language": "th",
                "pictureUrl": "https://profile.line-scdn.net/0h",
                "statusMessage": "ขายไข่",
            }),
        )],
    );

    let profile = fake.client().profile(USER).await.unwrap();

    assert_eq!(
        profile,
        Some(Profile {
            display_name: "ป้าแดง 🥚".to_owned(),
            picture_url: Some("https://profile.line-scdn.net/0h".to_owned()),
        })
    );
}

#[tokio::test]
async fn no_profile_for_users_who_are_not_friends() {
    let fake = Fake::start().await;
    fake.script(
        &format!("/v2/bot/profile/{USER}"),
        [Answer::error(404, "Not found")],
    );
    let client = fake.client();

    assert_eq!(client.profile(USER).await.unwrap(), None);

    // A profile without a picture
    fake.script(
        &format!("/v2/bot/profile/{USER}"),
        [Answer::json(
            200,
            json!({ "displayName": "สมชาย", "userId": USER }),
        )],
    );
    let profile = client.profile(USER).await.unwrap().unwrap();
    assert_eq!(profile.picture_url, None);
}

/// FR-NOT-07: the OA's basic ID, which the village group digest links to its chat with.
#[tokio::test]
async fn the_bot_info_has_the_basic_id() {
    let fake = Fake::start().await;
    fake.script(
        BOT_INFO,
        [Answer::json(
            200,
            json!({
                "userId": "Ub9952f8...",
                "basicId": "@216ruvtu",
                "displayName": "IconMarket",
                "chatMode": "bot",
                "markAsReadMode": "auto",
            }),
        )],
    );

    let info = fake.client().bot_info().await.unwrap();

    assert_eq!(info.basic_id, "@216ruvtu");
    let request = &fake.requests()[0];
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("GET", BOT_INFO)
    );
}

/// FR-ADM-07, NFR-COST-01: the push quota as LINE counts it, limited or not, for an admin to
/// see before sending an announcement.
#[tokio::test]
async fn the_quota_is_what_line_allows_and_has_counted() {
    let fake = Fake::start().await;
    let limit = |body| [Answer::json(200, body)];
    fake.script(QUOTA, limit(json!({ "type": "limited", "value": 300 })));
    fake.script(
        QUOTA_CONSUMPTION,
        [Answer::json(200, json!({ "totalUsage": 320 }))],
    );

    let quota = fake.client().quota().await.unwrap();

    let expected = Quota {
        limit: Some(300),
        used: 320,
    };
    assert_eq!(quota, expected);
    assert_eq!(quota.left(), Some(0));
    let mut asked: Vec<_> = fake
        .requests()
        .iter()
        .map(|request| (request.method.to_string(), request.path.clone()))
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            ("GET".to_owned(), QUOTA.to_owned()),
            ("GET".to_owned(), QUOTA_CONSUMPTION.to_owned()),
        ]
    );

    fake.script(QUOTA, limit(json!({ "type": "none" })));
    fake.script(
        QUOTA_CONSUMPTION,
        [Answer::json(200, json!({ "totalUsage": 12 }))],
    );
    let unlimited = fake.client().quota().await.unwrap();
    assert_eq!(
        unlimited,
        Quota {
            limit: None,
            used: 12
        }
    );
    assert_eq!(unlimited.left(), None);

    // Either call failing fails both
    fake.script(QUOTA, [Answer::error(500, "Internal Server Error")]);
    assert!(fake.client().quota().await.is_err());
}

// Rich menus (#17)

fn menu() -> RichMenu {
    RichMenu {
        size: Size {
            width: 2500,
            height: 843,
        },
        selected: true,
        name: "buyer".to_owned(),
        chat_bar_text: "เมนู".to_owned(),
        areas: vec![
            Area {
                bounds: Bounds {
                    x: 0,
                    y: 0,
                    width: 1250,
                    height: 843,
                },
                action: Action::postback(label("ซื้อของ"), "menu:shop"),
            },
            Area {
                bounds: Bounds {
                    x: 1250,
                    y: 0,
                    width: 1250,
                    height: 843,
                },
                action: Action::RichMenuSwitch {
                    label: label("ผู้ขาย"),
                    rich_menu_alias_id: "seller".to_owned(),
                    data: "menu:seller".to_owned(),
                },
            },
        ],
    }
}

#[tokio::test]
async fn rich_menus_are_created_and_given_their_image() {
    let fake = Fake::start().await;
    fake.script(
        "/v2/bot/richmenu",
        [Answer::json(
            200,
            json!({ "richMenuId": "richmenu-88c05ef6" }),
        )],
    );
    let client = fake.client();

    let id = client.create_rich_menu(&menu()).await.unwrap();
    client
        .upload_rich_menu_image(&id, ImageType::Png, b"png".to_vec())
        .await
        .unwrap();

    assert_eq!(id, "richmenu-88c05ef6");
    let requests = fake.requests();
    assert_eq!(requests[0].path, "/v2/bot/richmenu");
    assert_eq!(
        requests[0].json(),
        json!({
            "size": { "width": 2500, "height": 843 },
            "selected": true,
            "name": "buyer",
            "chatBarText": "เมนู",
            "areas": [
                {
                    "bounds": { "x": 0, "y": 0, "width": 1250, "height": 843 },
                    "action": { "type": "postback", "label": "ซื้อของ", "data": "menu:shop" },
                },
                {
                    "bounds": { "x": 1250, "y": 0, "width": 1250, "height": 843 },
                    "action": {
                        "type": "richmenuswitch",
                        "label": "ผู้ขาย",
                        "richMenuAliasId": "seller",
                        "data": "menu:seller",
                    },
                },
            ],
        })
    );
    let upload = &requests[1];
    assert_eq!(
        (upload.host, upload.path.as_str()),
        ("data", "/v2/bot/richmenu/richmenu-88c05ef6/content")
    );
    assert_eq!(upload.header("content-type"), Some("image/png"));
    assert_eq!(upload.body.as_ref(), b"png");
}

#[tokio::test]
async fn rich_menus_over_a_limit_are_not_created() {
    let fake = Fake::start().await;
    let client = fake.client();
    let narrow = RichMenu {
        size: Size {
            width: 1200,
            height: 843,
        },
        ..menu()
    };
    let long_bar = RichMenu {
        chat_bar_text: "เมนูของผู้ซื้อและผู้ขาย".to_owned(),
        ..menu()
    };
    let mut outside = menu();
    outside.areas[1].bounds.x = 1300;
    let mut location = menu();
    location.areas[0].action = Action::Location {
        label: label("ส่งตำแหน่ง"),
    };

    for (path, menu) in [
        ("size", narrow),
        ("chatBarText", long_bar),
        ("areas[1].bounds", outside),
        ("areas[0].action", location),
    ] {
        match client.create_rich_menu(&menu).await {
            Err(Error::Limit(e)) => assert_eq!(e.path, path, "{e}"),
            other => panic!("{path}: {other:?}"),
        }
    }
    let image = vec![0; limits::RICH_MENU_IMAGE_BYTES + 1];
    let error = client
        .upload_rich_menu_image("richmenu-1", ImageType::Jpeg, image)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Limit(_)), "{error:?}");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn rich_menus_are_listed_linked_and_aliased() {
    let fake = Fake::start().await;
    fake.script(
        "/v2/bot/richmenu/list",
        [Answer::json(
            200,
            json!({ "richmenus": [{
                "richMenuId": "richmenu-1",
                "name": "buyer",
                "size": { "width": 2500, "height": 843 },
                "selected": true,
                "chatBarText": "เมนู",
                "areas": [],
            }] }),
        )],
    );
    fake.script(
        "/v2/bot/richmenu/alias/list",
        [Answer::json(
            200,
            json!({ "aliases": [{ "richMenuAliasId": "buyer", "richMenuId": "richmenu-1" }] }),
        )],
    );
    let client = fake.client();
    let many: Vec<String> = (0..501).map(|i| format!("U{i:032x}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();

    let menus = client.rich_menus().await.unwrap();
    client.set_default_rich_menu("richmenu-1").await.unwrap();
    client.link_rich_menu(USER, "richmenu-2").await.unwrap();
    client.unlink_rich_menu(USER).await.unwrap();
    client
        .link_rich_menu_to_users("richmenu-2", &many)
        .await
        .unwrap();
    client
        .create_rich_menu_alias("seller", "richmenu-2")
        .await
        .unwrap();
    client
        .update_rich_menu_alias("seller", "richmenu-3")
        .await
        .unwrap();
    let aliases = client.rich_menu_aliases().await.unwrap();
    client.delete_rich_menu_alias("seller").await.unwrap();
    client.delete_rich_menu("richmenu-2").await.unwrap();

    assert_eq!(
        menus,
        [RichMenuInfo {
            rich_menu_id: "richmenu-1".to_owned(),
            name: "buyer".to_owned(),
        }]
    );
    assert_eq!(
        aliases,
        [Alias {
            rich_menu_alias_id: "buyer".to_owned(),
            rich_menu_id: "richmenu-1".to_owned(),
        }]
    );
    let requests = fake.requests();
    let calls: Vec<_> = requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(
        calls,
        [
            "GET /v2/bot/richmenu/list".to_owned(),
            "POST /v2/bot/user/all/richmenu/richmenu-1".to_owned(),
            format!("POST /v2/bot/user/{USER}/richmenu/richmenu-2"),
            format!("DELETE /v2/bot/user/{USER}/richmenu"),
            "POST /v2/bot/richmenu/bulk/link".to_owned(),
            "POST /v2/bot/richmenu/bulk/link".to_owned(),
            "POST /v2/bot/richmenu/alias".to_owned(),
            "POST /v2/bot/richmenu/alias/seller".to_owned(),
            "GET /v2/bot/richmenu/alias/list".to_owned(),
            "DELETE /v2/bot/richmenu/alias/seller".to_owned(),
            "DELETE /v2/bot/richmenu/richmenu-2".to_owned(),
        ]
    );
    let bulk: Vec<usize> = requests[4..6]
        .iter()
        .map(|r| r.json()["userIds"].as_array().unwrap().len())
        .collect();
    assert_eq!(bulk, [500, 1]);
    assert_eq!(requests[4].json()["richMenuId"], "richmenu-2");
    assert_eq!(
        requests[6].json(),
        json!({ "richMenuAliasId": "seller", "richMenuId": "richmenu-2" })
    );
    assert_eq!(requests[7].json(), json!({ "richMenuId": "richmenu-3" }));
}

// What goes over the wire

#[test]
fn messages_serialize_as_line_expects() {
    let text = Message::text("ต้องการกี่แผงคะ").with_quick_reply(QuickReply::new([
        Action::postback(label("1 แผง"), "qty:1"),
        Action::Postback {
            label: label("2 แผง"),
            data: "qty:2".to_owned(),
            display_text: Some("2 แผง".to_owned()),
        },
        Action::CameraRoll {
            label: label("เลือกรูป"),
        },
    ]));
    assert_eq!(
        serde_json::to_value(&text).unwrap(),
        json!({
            "type": "text",
            "text": "ต้องการกี่แผงคะ",
            "quickReply": { "items": [
                { "type": "action", "action": { "type": "postback", "label": "1 แผง", "data": "qty:1" } },
                {
                    "type": "action",
                    "action": { "type": "postback", "label": "2 แผง", "data": "qty:2", "displayText": "2 แผง" },
                },
                { "type": "action", "action": { "type": "cameraRoll", "label": "เลือกรูป" } },
            ] },
        })
    );

    let image = Message::image(&https("1"), &https("1?preview"));
    assert_eq!(
        serde_json::to_value(&image).unwrap(),
        json!({
            "type": "image",
            "originalContentUrl": "https://market.example.com/img/p/1",
            "previewImageUrl": "https://market.example.com/img/p/1?preview",
        })
    );
}

#[test]
fn cards_serialize_as_line_expects() {
    let card = Bubble {
        size: Some(BubbleSize::Kilo),
        hero: Some(flex::Hero::Image(Image {
            size: Some(TextSize::Full),
            aspect_ratio: Some("20:13".to_owned()),
            aspect_mode: Some(flex::AspectMode::Cover),
            ..Image::new(&https("7"))
        })),
        body: Some(FlexBox {
            spacing: Some(flex::Spacing::Sm),
            ..FlexBox::vertical([
                Text {
                    weight: Some(Weight::Bold),
                    size: Some(TextSize::Xl),
                    wrap: Some(true),
                    ..Text::new("ไข่ไก่สด")
                }
                .into(),
                flex::Separator::default().into(),
                FlexBox::baseline([Text::new("฿120").into()]).into(),
            ])
        }),
        footer: Some(FlexBox::vertical([Button {
            style: Some(ButtonStyle::Primary),
            ..Button::new(Action::postback(label("ใส่ตะกร้า"), "cart:add:7"))
        }
        .into()])),
        ..Bubble::default()
    };
    let message = Message::flex("ไข่ไก่สด ฿120", Carousel::new(vec![card]));

    assert_eq!(
        serde_json::to_value(&message).unwrap(),
        json!({
            "type": "flex",
            "altText": "ไข่ไก่สด ฿120",
            "contents": {
                "type": "carousel",
                "contents": [{
                    "type": "bubble",
                    "size": "kilo",
                    "hero": {
                        "type": "image",
                        "url": "https://market.example.com/img/p/7",
                        "size": "full",
                        "aspectRatio": "20:13",
                        "aspectMode": "cover",
                    },
                    "body": {
                        "type": "box",
                        "layout": "vertical",
                        "spacing": "sm",
                        "contents": [
                            { "type": "text", "text": "ไข่ไก่สด", "weight": "bold", "size": "xl", "wrap": true },
                            { "type": "separator" },
                            {
                                "type": "box",
                                "layout": "baseline",
                                "contents": [{ "type": "text", "text": "฿120" }],
                            },
                        ],
                    },
                    "footer": {
                        "type": "box",
                        "layout": "vertical",
                        "contents": [{
                            "type": "button",
                            "style": "primary",
                            "action": { "type": "postback", "label": "ใส่ตะกร้า", "data": "cart:add:7" },
                        }],
                    },
                }],
            },
        })
    );
}

// Secrets and personal data (CFG-06, NFR-PRIV-05)

#[tokio::test]
async fn the_token_is_never_shown() {
    let fake = Fake::start().await;
    let client = fake.client();
    assert!(!format!("{client:?}").contains(TOKEN));

    let config = crate::config::LineConfig {
        channel_secret: Secret::new("channel-secret-value"),
        channel_access_token: Secret::new(TOKEN),
        rich_menu: crate::config::RichMenuConfig {
            auto_provision: true,
        },
        village_group_id: None,
        api_base_url: None,
        data_base_url: None,
    };
    let client = Client::new(&config).unwrap();
    assert!(!format!("{client:?}").contains(TOKEN));
}

#[tokio::test]
async fn errors_carry_no_ids_or_urls() {
    // Nothing listens on port 1
    let nowhere = Url::parse("http://127.0.0.1:1/").unwrap();
    let client = Client::for_test(&Secret::new(TOKEN), nowhere.clone(), nowhere);

    let error = client.profile(USER).await.unwrap_err();

    assert!(matches!(error, Error::Network(_)), "{error:?}");
    let shown = format!("{error} {error:?}");
    // LINE's own address may show; the user ID and the URL holding it may not
    for leak in [USER, "profile", TOKEN] {
        assert!(!shown.contains(leak), "{leak} in {shown}");
    }
    assert!(error.is_retryable());
}
