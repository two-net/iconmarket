//! Dispatcher (SRS §2.1): processes inbox events in order per user and routes input
//! (§3.1.3). Built by #13 and #14.

use chrono::TimeDelta;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

use crate::persistence::entity::{pending_action, session};

/// How long an expired session is kept, so that a user who comes back the same day is told
/// their step expired (FR-UI-15) instead of finding no trace of it.
const EXPIRED_SESSION_GRACE: TimeDelta = TimeDelta::days(1);

/// What [`delete_expired`] deleted.
#[derive(Debug, PartialEq, Eq)]
pub struct Deleted {
    pub sessions: u64,
    pub pending_actions: u64,
}

/// Deletes the sessions that expired more than [`EXPIRED_SESSION_GRACE`] before `now`, and the
/// pending actions that expired before `now`, consumed or not (§5.4). A tap on a deleted
/// action's button finds nothing, so it shows the current state (FR-UI-13).
pub async fn delete_expired(db: &impl ConnectionTrait, now: DateTimeUtc) -> Result<Deleted, DbErr> {
    let sessions = session::Entity::delete_many()
        .filter(session::Column::ExpiresAt.lt(now - EXPIRED_SESSION_GRACE))
        .exec(db)
        .await?
        .rows_affected;
    let pending_actions = pending_action::Entity::delete_many()
        .filter(pending_action::Column::ExpiresAt.lt(now))
        .exec(db)
        .await?
        .rows_affected;
    Ok(Deleted {
        sessions,
        pending_actions,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use sea_orm::ActiveValue::Set;
    use sea_orm::prelude::Uuid;
    use sea_orm::{ActiveModelTrait, DatabaseConnection, QueryOrder, QuerySelect};
    use serde_json::json;

    use super::*;
    use crate::persistence::entity::user;
    use crate::persistence::{orm, random_id, testing};

    async fn new_user(db: &DatabaseConnection, line_user_id: &str) -> i64 {
        let now = Utc::now();
        let user = user::ActiveModel {
            line_user_id: Set(Some(line_user_id.to_owned())),
            is_admin: Set(false),
            is_following: Set(true),
            status: Set(user::Status::Active),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        user.insert(db).await.unwrap().id
    }

    async fn new_session(db: &DatabaseConnection, user_id: i64, expires_at: DateTimeUtc) {
        let session = session::ActiveModel {
            user_id: Set(user_id),
            flow: Set("add_product".to_owned()),
            step: Set("price".to_owned()),
            data: Set(json!({ "name": "ไข่ไก่" })),
            expires_at: Set(expires_at),
        };
        session.insert(db).await.unwrap();
    }

    async fn new_pending_action(
        db: &DatabaseConnection,
        user_id: i64,
        expires_at: DateTimeUtc,
        consumed: bool,
    ) -> Uuid {
        let action = pending_action::ActiveModel {
            id: Set(random_id()),
            user_id: Set(user_id),
            kind: Set("checkout".to_owned()),
            payload: Set(json!({ "shop_id": 1 })),
            expires_at: Set(expires_at),
            consumed_at: Set(consumed.then_some(expires_at - TimeDelta::minutes(30))),
            created_at: Set(expires_at - TimeDelta::hours(1)),
        };
        action.insert(db).await.unwrap().id
    }

    async fn sessions(db: &DatabaseConnection) -> Vec<i64> {
        session::Entity::find()
            .select_only()
            .column(session::Column::UserId)
            .order_by_asc(session::Column::UserId)
            .into_tuple()
            .all(db)
            .await
            .unwrap()
    }

    async fn pending_actions(db: &DatabaseConnection) -> Vec<Uuid> {
        pending_action::Entity::find()
            .select_only()
            .column(pending_action::Column::Id)
            .into_tuple()
            .all(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn expired_sessions_and_pending_actions_are_deleted() {
        let (_container, pool) = testing::migrated().await;
        let db = orm(pool);
        let now = Utc.with_ymd_and_hms(2026, 10, 24, 7, 30, 0).unwrap();
        let second = TimeDelta::seconds(1);

        let [gone, expired_today, active] = [
            "U0123456789abcdef0123456789abcde0",
            "U0123456789abcdef0123456789abcde1",
            "U0123456789abcdef0123456789abcde2",
        ];
        let (gone, expired_today, active) = (
            new_user(&db, gone).await,
            new_user(&db, expired_today).await,
            new_user(&db, active).await,
        );
        new_session(&db, gone, now - EXPIRED_SESSION_GRACE - second).await;
        new_session(&db, expired_today, now - EXPIRED_SESSION_GRACE + second).await;
        new_session(&db, active, now + TimeDelta::minutes(15)).await;
        new_pending_action(&db, active, now - second, false).await;
        new_pending_action(&db, active, now - second, true).await;
        let open = new_pending_action(&db, active, now + second, false).await;

        let deleted = delete_expired(&db, now).await.unwrap();
        assert_eq!(
            deleted,
            Deleted {
                sessions: 1,
                pending_actions: 2
            }
        );
        assert_eq!(sessions(&db).await, [expired_today, active]);
        assert_eq!(pending_actions(&db).await, [open]);

        // Running it again has no further effect
        let again = delete_expired(&db, now).await.unwrap();
        assert_eq!(
            again,
            Deleted {
                sessions: 0,
                pending_actions: 0
            }
        );
        assert_eq!(sessions(&db).await, [expired_today, active]);
        assert_eq!(pending_actions(&db).await, [open]);
    }
}
