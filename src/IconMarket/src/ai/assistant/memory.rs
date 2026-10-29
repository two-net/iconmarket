//! Conversation memory (FR-AI-11): each message to the assistant and what it answered, kept per
//! user in `ai_messages` until the retention job deletes it after `retention.aiMessageDays`, the
//! last `ai.historyTurns` sent with the next message. `เริ่มใหม่` [clears](clear) it, and so does
//! deleting the account (FR-ONB-07).
//!
//! A turn is kept as three rows: what the user wrote; what they were shown, by ID and name only,
//! so that *เอาอันแรก* finds what *อันแรก* was; and what the assistant said. Calls to tools and
//! their results are not kept: some providers need a call and its result in the same request,
//! and prices, stock and statuses are read afresh from the database each time (FR-AI-04). What
//! the user wrote, and what they were shown, go back to the model as data (FR-AI-06).

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde_json::{Value, json};

use crate::ai::{self, as_data};
use crate::persistence::entity::ai_message::{self, Role};

/// The tag of what the user wrote, as the model is given it
const SAID: &str = "message";
/// The tag of what they were shown
const SHOWN: &str = "shown";

/// What the user wrote, `text`, as the model is given it: data, never instructions (FR-AI-06).
pub fn said(text: &str) -> ai::Message {
    ai::Message::user(as_data(SAID, &json!({ "text": text })))
}

/// The last `turns` turns of user `user_id`'s conversation, oldest first, as the model is given
/// them; none if `turns` is 0.
pub async fn recall(
    db: &impl ConnectionTrait,
    user_id: i64,
    turns: u32,
) -> Result<Vec<ai::Message>, DbErr> {
    if turns == 0 {
        return Ok(Vec::new());
    }
    // Each turn starts with what the user wrote: from the oldest of the last `turns` of those
    let first: Option<i64> = ai_message::Entity::find()
        .select_only()
        .column(ai_message::Column::Id)
        .filter(ai_message::Column::UserId.eq(user_id))
        .filter(ai_message::Column::Role.eq(Role::User))
        .order_by_desc(ai_message::Column::Id)
        .offset(u64::from(turns) - 1)
        .limit(1)
        .into_tuple()
        .one(db)
        .await?;
    let rows = ai_message::Entity::find()
        .filter(ai_message::Column::UserId.eq(user_id))
        .filter(ai_message::Column::Id.gte(first.unwrap_or(0)))
        .order_by_asc(ai_message::Column::Id)
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| match row.role {
            Role::User => said(&row.content),
            Role::Tool => {
                let shown = serde_json::from_str(&row.content).unwrap_or(Value::Null);
                ai::Message::user(as_data(SHOWN, &shown))
            }
            Role::Assistant => ai::Message::assistant(row.content),
        })
        .collect())
}

/// Keeps a turn of user `user_id`'s at `now`: what they `said`, what they were `shown` and what
/// the assistant `answered`.
pub async fn remember(
    db: &impl ConnectionTrait,
    user_id: i64,
    now: DateTimeUtc,
    said: &str,
    shown: &Value,
    answered: &str,
) -> Result<(), DbErr> {
    let row = |role, content: String| ai_message::ActiveModel {
        user_id: Set(user_id),
        role: Set(role),
        content: Set(content),
        created_at: Set(now),
        ..Default::default()
    };
    let rows = [
        row(Role::User, said.to_owned()),
        row(Role::Tool, shown.to_string()),
        row(Role::Assistant, answered.to_owned()),
    ];
    ai_message::Entity::insert_many(rows)
        .exec_without_returning(db)
        .await?;
    Ok(())
}

/// Forgets user `user_id`'s conversation: `เริ่มใหม่` (FR-AI-11).
pub async fn clear(db: &impl ConnectionTrait, user_id: i64) -> Result<(), DbErr> {
    ai_message::Entity::delete_many()
        .filter(ai_message::Column::UserId.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}
