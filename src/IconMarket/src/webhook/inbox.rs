//! Storing webhook events in the inbox (FR-LINE-03, FR-LINE-04): once each, keyed by LINE's
//! `webhookEventId`, so a redelivery adds nothing and leaves the stored event as it is.

use std::fmt;

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{DbErr, EntityTrait, TryInsertResult};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;

use crate::logging;
use crate::persistence::{self, entity::webhook_event};

/// An event as LINE sent it, with the fields the inbox keeps it by.
#[derive(Debug)]
pub struct Event {
    id: String,
    event_type: String,
    user_key: String,
    payload: Value,
}

/// A webhook body that is not what LINE sends. Says where, but not what the body held, so it
/// can be logged (NFR-PRIV-05).
#[derive(Debug, PartialEq, Eq)]
pub enum Malformed {
    Body,
    Event(usize),
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Body => f.write_str("the body is not JSON with an `events` array"),
            Self::Event(index) => write!(
                f,
                "event {index} lacks a string `webhookEventId` or `type`, or has a malformed `source`"
            ),
        }
    }
}

#[derive(Deserialize)]
struct Body {
    events: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fields {
    webhook_event_id: String,
    #[serde(rename = "type")]
    event_type: String,
    source: Option<Source>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    user_id: Option<String>,
    group_id: Option<String>,
    room_id: Option<String>,
}

impl Source {
    /// The chat the event came from: the group or room, or else the user. Their IDs start
    /// with C, R and U, so they never collide.
    fn key(self) -> Option<String> {
        self.group_id.or(self.room_id).or(self.user_id)
    }
}

/// The events of a webhook body, in the order LINE sent them. A body that is not what LINE
/// sends is refused whole, so nothing of it is stored.
pub fn parse(body: &[u8]) -> Result<Vec<Event>, Malformed> {
    let body: Body = serde_json::from_slice(body).map_err(|_| Malformed::Body)?;
    body.events
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            let fields = Fields::deserialize(&payload).map_err(|_| Malformed::Event(index))?;
            Ok(Event {
                id: fields.webhook_event_id,
                event_type: fields.event_type,
                // Some event types may come without a source
                user_key: fields.source.and_then(Source::key).unwrap_or_default(),
                payload,
            })
        })
        .collect()
}

/// Stores `events` as pending, in one statement so that either all are stored or none are.
/// Events already in the inbox are left as they are.
pub async fn store(db: &PgPool, events: Vec<Event>) -> Result<(), DbErr> {
    let received_at = Utc::now();
    let rows = events.iter().map(|event| webhook_event::ActiveModel {
        event_id: Set(event.id.clone()),
        user_key: Set(event.user_key.clone()),
        event_type: Set(event.event_type.clone()),
        payload: Set(event.payload.clone()),
        received_at: Set(received_at),
        status: Set(webhook_event::Status::Pending),
        attempts: Set(0),
        ..Default::default()
    });
    let inserted = webhook_event::Entity::insert_many(rows)
        .on_conflict_do_nothing_on([webhook_event::Column::EventId])
        .exec_with_returning_many(&persistence::orm(db.clone()))
        .await?;
    let inserted = match inserted {
        TryInsertResult::Inserted(rows) => rows,
        TryInsertResult::Conflicted | TryInsertResult::Empty => Vec::new(),
    };

    for event in &events {
        let duplicate = !inserted.iter().any(|row| row.event_id == event.id);
        logging::event_span(&event.id).in_scope(|| {
            debug!(event_type = %event.event_type, duplicate, "webhook event received");
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn keys(events: &[Value]) -> Vec<String> {
        let body = json!({ "destination": "U0", "events": events }).to_string();
        parse(body.as_bytes())
            .unwrap()
            .into_iter()
            .map(|event| event.user_key)
            .collect()
    }

    #[test]
    fn events_are_keyed_by_the_chat_they_came_from() {
        let event = |source: Value| json!({ "type": "message", "webhookEventId": "01J0", "source": source });
        let keys = keys(&[
            event(json!({ "type": "user", "userId": "U1" })),
            event(json!({ "type": "group", "groupId": "C1", "userId": "U1" })),
            event(json!({ "type": "room", "roomId": "R1", "userId": "U1" })),
            event(json!({ "type": "group", "groupId": "C1" })),
            json!({ "type": "unknown", "webhookEventId": "01J0" }),
        ]);
        assert_eq!(keys, ["U1", "C1", "R1", "C1", ""]);
    }

    #[test]
    fn events_keep_their_order_and_their_whole_payload() {
        let first = json!({
            "type": "follow",
            "webhookEventId": "01J1",
            "deliveryContext": { "isRedelivery": false },
            "source": { "type": "user", "userId": "U1" },
            "timestamp": 1_761_300_000_000_u64,
        });
        let second = json!({ "type": "unfollow", "webhookEventId": "01J2" });
        let body = json!({ "events": [first, second] }).to_string();

        let events = parse(body.as_bytes()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            (events[0].id.as_str(), events[0].event_type.as_str()),
            ("01J1", "follow")
        );
        assert_eq!(events[0].payload, first);
        assert_eq!(events[1].id, "01J2");
        assert_eq!(events[1].payload, second);
    }

    #[test]
    fn bodies_that_are_not_from_line_are_refused_whole() {
        let good = json!({ "type": "follow", "webhookEventId": "01J1" });
        for (body, expected) in [
            ("".to_owned(), Malformed::Body),
            ("not json".to_owned(), Malformed::Body),
            (json!({ "destination": "U0" }).to_string(), Malformed::Body),
            (json!({ "events": {} }).to_string(), Malformed::Body),
            (json!({ "events": [good, 7] }).to_string(), Malformed::Event(1)),
            (
                json!({ "events": [good, { "type": "follow" }] }).to_string(),
                Malformed::Event(1),
            ),
            (
                json!({ "events": [{ "type": "follow", "webhookEventId": 1 }] }).to_string(),
                Malformed::Event(0),
            ),
            (
                json!({ "events": [{ "webhookEventId": "01J1" }] }).to_string(),
                Malformed::Event(0),
            ),
            (
                json!({ "events": [{ "type": "follow", "webhookEventId": "01J1", "source": "U1" }] })
                    .to_string(),
                Malformed::Event(0),
            ),
        ] {
            assert_eq!(parse(body.as_bytes()).unwrap_err(), expected, "{body}");
        }
    }

    #[test]
    fn an_empty_event_list_is_valid() {
        let body = json!({ "destination": "U0", "events": [] }).to_string();
        assert!(parse(body.as_bytes()).unwrap().is_empty());
    }
}
