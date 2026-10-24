//! The audit log (FR-ADM-08, NFR-SEC-09): every admin action and sensitive action, with who did
//! it, what, to which target, when and why.
//!
//! Entries are written on the caller's connection, which is normally the transaction making
//! the change, so the change and its entry are committed together: an action that can't be
//! audited doesn't happen.

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait};
use serde_json::json;
use tracing::info;

use crate::persistence::entity::audit_log;

/// What was done. Each flow adds its actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The admin role was given to a user listed in `admin.lineUserIds` (FR-ADM-01)
    AdminGranted,
    /// The admin role was taken from a user no longer listed
    AdminRevoked,
}

impl Action {
    /// Its name in `audit_logs.action`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdminGranted => "admin.granted",
            Self::AdminRevoked => "admin.revoked",
        }
    }
}

/// What it was done to. Each flow adds the kinds of target it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    User(i64),
}

impl Target {
    /// Its `audit_logs.target_type` and `target_id`.
    fn parts(self) -> (&'static str, i64) {
        match self {
            Self::User(id) => ("user", id),
        }
    }
}

/// One audited action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Who did it; `None` for the system
    pub actor_id: Option<i64>,
    pub action: Action,
    pub target: Target,
    /// Why: the admin's reason, or the system's
    pub reason: Option<String>,
}

/// Writes `entries`, done at `now`, to the audit log. The reason goes in `details.reason`, and
/// is not logged: an admin's reason can name people (NFR-PRIV-05).
pub async fn record(
    db: &impl ConnectionTrait,
    now: DateTimeUtc,
    entries: impl IntoIterator<Item = Entry>,
) -> Result<(), DbErr> {
    let entries: Vec<_> = entries.into_iter().collect();
    if entries.is_empty() {
        return Ok(());
    }
    let rows = entries.iter().map(|entry| {
        let (target_type, target_id) = entry.target.parts();
        let details = match &entry.reason {
            Some(reason) => json!({ "reason": reason }),
            None => json!({}),
        };
        audit_log::ActiveModel {
            actor_id: Set(entry.actor_id),
            action: Set(entry.action.as_str().to_owned()),
            target_type: Set(target_type.to_owned()),
            target_id: Set(Some(target_id)),
            details: Set(details),
            created_at: Set(now),
            ..Default::default()
        }
    });
    audit_log::Entity::insert_many(rows).exec(db).await?;
    for entry in &entries {
        let (target_type, target_id) = entry.target.parts();
        info!(
            actor_id = entry.actor_id,
            action = entry.action.as_str(),
            target_type,
            target_id,
            "audited"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::prelude::Json;
    use sea_orm::{ActiveModelTrait, DatabaseConnection, QueryOrder, TransactionTrait};

    use super::*;
    use crate::persistence::entity::user;
    use crate::persistence::orm;
    use crate::persistence::testing::{self, at, new_user};

    /// Every row as (actor, action, target type, target, details, time), oldest first.
    async fn audited(
        db: &DatabaseConnection,
    ) -> Vec<(Option<i64>, String, String, Option<i64>, Json, DateTimeUtc)> {
        audit_log::Entity::find()
            .order_by_asc(audit_log::Column::Id)
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                let audit_log::Model {
                    actor_id,
                    action,
                    target_type,
                    target_id,
                    details,
                    created_at,
                    ..
                } = row;
                (
                    actor_id,
                    action,
                    target_type,
                    target_id,
                    details,
                    created_at,
                )
            })
            .collect()
    }

    /// FR-ADM-08: who, what, target, when and why, committed with the change or not at all.
    #[tokio::test]
    async fn entries_are_written_with_their_change_or_not_at_all() {
        let (_container, pool) = testing::migrated().await;
        let db = orm(pool);
        let admin = user::ActiveModel {
            is_admin: Set(true),
            ..new_user("U-admin")
        }
        .insert(&db)
        .await
        .unwrap();
        let target = new_user("U-target").insert(&db).await.unwrap();
        let by_admin = Entry {
            actor_id: Some(admin.id),
            action: Action::AdminRevoked,
            target: Target::User(target.id),
            reason: Some("ทดสอบระบบ".to_owned()),
        };
        let by_system = Entry {
            actor_id: None,
            action: Action::AdminGranted,
            target: Target::User(target.id),
            reason: None,
        };

        let txn = db.begin().await.unwrap();
        record(&txn, at(), [by_admin.clone()]).await.unwrap();
        txn.rollback().await.unwrap();
        assert_eq!(audited(&db).await, []);

        let txn = db.begin().await.unwrap();
        record(&txn, at(), [by_admin, by_system]).await.unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            audited(&db).await,
            [
                (
                    Some(admin.id),
                    "admin.revoked".to_owned(),
                    "user".to_owned(),
                    Some(target.id),
                    json!({ "reason": "ทดสอบระบบ" }),
                    at(),
                ),
                (
                    None,
                    "admin.granted".to_owned(),
                    "user".to_owned(),
                    Some(target.id),
                    json!({}),
                    at(),
                ),
            ]
        );

        // Nothing to record writes nothing
        record(&db, at(), []).await.unwrap();
        assert_eq!(audited(&db).await.len(), 2);
    }
}
