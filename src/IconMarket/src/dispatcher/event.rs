//! Webhook events as the dispatcher reads them from the inbox (FR-LINE-06): what happened, in
//! which chat, and how to answer. LINE's format is at
//! <https://developers.line.biz/en/reference/messaging-api/#webhook-event-objects>.
//!
//! Reading never fails. The webhook stored the event whole, and LINE may add types and fields
//! (A-07), so an event of a type the dispatcher doesn't handle, or one missing what its type
//! needs, reads as [`Kind::Other`] and is ignored, and a message of another kind reads as
//! [`Content::Unsupported`] and gets a friendly reply (FR-UI-09).

use serde::Deserialize;
use serde_json::Value;

/// An event, as far as the dispatcher needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Answers the event once, within about a minute (FR-LINE-07); `None` for events LINE
    /// gives none, such as `unfollow` and `leave`
    pub reply_token: Option<String>,
    pub source: Source,
    pub kind: Kind,
}

/// The chat an event came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The user's one-to-one chat with the OA
    User(String),
    /// A group chat, with its member who caused the event if LINE says
    Group {
        group_id: String,
        user_id: Option<String>,
    },
    /// A multi-person chat
    Room,
    /// No source, or one LINE doesn't document
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Follow,
    Unfollow,
    Message(Content),
    /// A postback button was tapped; its data is anyone's to send (NFR-SEC-03)
    Postback(String),
    /// The OA was added to a group
    Join,
    /// The OA was removed from a group
    Leave,
    /// Any other type, which is acknowledged and ignored
    Other,
}

/// What a message holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    /// A photo, downloaded by its message ID while the event is processed (FR-LINE-09)
    Image {
        message_id: String,
    },
    Location(Location),
    /// A voice message, downloaded like a photo
    Audio {
        message_id: String,
    },
    /// Stickers, videos, files and any kind LINE adds
    Unsupported,
}

/// A location the user shared.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fields {
    reply_token: Option<String>,
    source: Option<Value>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RawKind {
    Follow,
    Unfollow,
    Message {
        message: Value,
    },
    Postback {
        postback: RawPostback,
    },
    Join,
    Leave,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct RawPostback {
    data: String,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum RawSource {
    User {
        user_id: String,
    },
    Group {
        group_id: String,
        user_id: Option<String>,
    },
    Room,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RawContent {
    Text {
        text: String,
    },
    Image {
        id: String,
    },
    Location {
        latitude: f64,
        longitude: f64,
        address: Option<String>,
    },
    Audio {
        id: String,
    },
    #[serde(other)]
    Unsupported,
}

impl Event {
    /// The event a stored payload holds.
    pub fn read(payload: &Value) -> Self {
        let fields = Fields::deserialize(payload).unwrap_or(Fields {
            reply_token: None,
            source: None,
        });
        let source = fields
            .source
            .and_then(|source| RawSource::deserialize(source).ok())
            .map_or(Source::Unknown, Source::from);
        let kind = RawKind::deserialize(payload).map_or(Kind::Other, |kind| match kind {
            RawKind::Follow => Kind::Follow,
            RawKind::Unfollow => Kind::Unfollow,
            RawKind::Message { message } => Kind::Message(Content::read(message)),
            RawKind::Postback { postback } => Kind::Postback(postback.data),
            RawKind::Join => Kind::Join,
            RawKind::Leave => Kind::Leave,
            RawKind::Other => Kind::Other,
        });
        Self {
            reply_token: fields.reply_token,
            source,
            kind,
        }
    }
}

impl From<RawSource> for Source {
    fn from(source: RawSource) -> Self {
        match source {
            RawSource::User { user_id } => Self::User(user_id),
            RawSource::Group { group_id, user_id } => Self::Group { group_id, user_id },
            RawSource::Room => Self::Room,
            RawSource::Unknown => Self::Unknown,
        }
    }
}

impl Content {
    fn read(message: Value) -> Self {
        match RawContent::deserialize(message) {
            Ok(RawContent::Text { text }) => Self::Text(text),
            Ok(RawContent::Image { id }) => Self::Image { message_id: id },
            Ok(RawContent::Location {
                latitude,
                longitude,
                address,
            }) => Self::Location(Location {
                latitude,
                longitude,
                address,
            }),
            Ok(RawContent::Audio { id }) => Self::Audio { message_id: id },
            Ok(RawContent::Unsupported) | Err(_) => Self::Unsupported,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const USER: &str = "U0123456789abcdef0123456789abcdef";
    const GROUP: &str = "C0123456789abcdef0123456789abcdef";

    fn user() -> Value {
        json!({ "type": "user", "userId": USER })
    }

    fn message(message: Value) -> Value {
        json!({
            "type": "message",
            "mode": "active",
            "timestamp": 1_761_300_000_000_u64,
            "webhookEventId": "01J0",
            "deliveryContext": { "isRedelivery": false },
            "replyToken": "reply-1",
            "source": user(),
            "message": message,
        })
    }

    fn content(message: Value) -> Content {
        match Event::read(&self::message(message)).kind {
            Kind::Message(content) => content,
            kind => panic!("not a message: {kind:?}"),
        }
    }

    #[test]
    fn messages_are_read_with_their_reply_token_and_sender() {
        let event = Event::read(&message(json!({
            "id": "468789577898262530",
            "type": "text",
            "quoteToken": "q",
            "text": "เมนู",
        })));
        assert_eq!(
            event,
            Event {
                reply_token: Some("reply-1".to_owned()),
                source: Source::User(USER.to_owned()),
                kind: Kind::Message(Content::Text("เมนู".to_owned())),
            }
        );
    }

    #[test]
    fn each_kind_of_message_is_read() {
        let image = json!({ "id": "1", "type": "image", "contentProvider": { "type": "line" } });
        assert_eq!(
            content(image),
            Content::Image {
                message_id: "1".to_owned()
            }
        );
        let audio = json!({ "id": "2", "type": "audio", "duration": 6000 });
        assert_eq!(
            content(audio),
            Content::Audio {
                message_id: "2".to_owned()
            }
        );
        let location = json!({
            "id": "3",
            "type": "location",
            "title": "บ้าน",
            "address": "ซอย 1",
            "latitude": 13.75,
            "longitude": 100.5,
        });
        assert_eq!(
            content(location),
            Content::Location(Location {
                latitude: 13.75,
                longitude: 100.5,
                address: Some("ซอย 1".to_owned()),
            })
        );
        let pinned = json!({ "id": "4", "type": "location", "latitude": 13.0, "longitude": 100.0 });
        assert!(matches!(
            content(pinned),
            Content::Location(Location { address: None, .. })
        ));
    }

    /// FR-UI-09: what the dispatcher can't take reads as unsupported, however it is malformed.
    #[test]
    fn other_messages_are_unsupported() {
        for message in [
            json!({ "id": "1", "type": "sticker", "packageId": "446", "stickerId": "1988" }),
            json!({ "id": "2", "type": "video", "duration": 60000 }),
            json!({ "id": "3", "type": "file", "fileName": "a.pdf", "fileSize": 1 }),
            json!({ "id": "4", "type": "hologram" }),
            json!({ "id": "5", "type": "text" }),
            json!({ "id": "6", "type": "location", "latitude": "north" }),
            json!({ "id": "7" }),
            json!("text"),
        ] {
            assert_eq!(content(message.clone()), Content::Unsupported, "{message}");
        }
    }

    #[test]
    fn postbacks_follows_and_group_events_are_read() {
        let postback = json!({
            "type": "postback",
            "replyToken": "reply-2",
            "source": user(),
            "postback": { "data": "browse?page=2", "params": {} },
        });
        let event = Event::read(&postback);
        assert_eq!(event.kind, Kind::Postback("browse?page=2".to_owned()));
        assert_eq!(event.reply_token.as_deref(), Some("reply-2"));

        let follow = json!({
            "type": "follow",
            "replyToken": "reply-3",
            "source": user(),
            "follow": { "isUnblocked": false },
        });
        assert_eq!(Event::read(&follow).kind, Kind::Follow);
        let unfollow = json!({ "type": "unfollow", "source": user() });
        let unfollow = Event::read(&unfollow);
        assert_eq!(
            (unfollow.kind, unfollow.reply_token),
            (Kind::Unfollow, None)
        );

        let in_group = json!({ "type": "group", "groupId": GROUP, "userId": USER });
        let join = json!({ "type": "join", "replyToken": "r", "source": { "type": "group", "groupId": GROUP } });
        let join = Event::read(&join);
        assert_eq!(join.kind, Kind::Join);
        assert_eq!(
            join.source,
            Source::Group {
                group_id: GROUP.to_owned(),
                user_id: None
            }
        );
        let said = Event::read(&json!({
            "type": "message",
            "source": in_group,
            "message": { "id": "1", "type": "text", "text": "#groupid" },
        }));
        assert_eq!(
            said.source,
            Source::Group {
                group_id: GROUP.to_owned(),
                user_id: Some(USER.to_owned())
            }
        );
        let leave = json!({ "type": "leave", "source": { "type": "group", "groupId": GROUP } });
        assert_eq!(Event::read(&leave).kind, Kind::Leave);
        let room = json!({ "type": "message", "source": { "type": "room", "roomId": "R1", "userId": USER }, "message": { "id": "1", "type": "text", "text": "hi" } });
        assert_eq!(Event::read(&room).source, Source::Room);
    }

    /// FR-LINE-06: other types, and events missing what their type needs, are ignored.
    #[test]
    fn anything_else_reads_as_other() {
        for payload in [
            json!({ "type": "memberJoined", "replyToken": "r", "source": user() }),
            json!({ "type": "unsend", "source": user(), "unsend": { "messageId": "1" } }),
            json!({ "type": "videoPlayComplete", "source": user() }),
            json!({ "type": "message", "source": user() }),
            json!({ "type": "postback", "source": user(), "postback": {} }),
            json!({ "type": "postback", "source": user(), "postback": { "data": 7 } }),
            json!({ "webhookEventId": "01J0" }),
            json!([]),
        ] {
            assert_eq!(Event::read(&payload).kind, Kind::Other, "{payload}");
        }
    }

    #[test]
    fn missing_or_unknown_sources_are_unknown() {
        for source in [
            None,
            Some(json!({ "type": "user" })),
            Some(json!({ "type": "group" })),
            Some(json!({ "type": "channel", "channelId": "1" })),
            Some(json!("U1")),
        ] {
            let mut payload = json!({ "type": "follow", "replyToken": "r" });
            if let Some(source) = source {
                payload["source"] = source;
            }
            assert_eq!(Event::read(&payload).source, Source::Unknown, "{payload}");
        }
    }
}
