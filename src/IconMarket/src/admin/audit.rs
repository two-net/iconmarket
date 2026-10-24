//! The audit log (FR-ADM-08, NFR-SEC-09): every admin action and sensitive action, with who did
//! it, what, to which target, when and why.
//!
//! Entries are written on the caller's connection, which is normally the transaction making
//! the change, so the change and its entry are committed together: an action that can't be
//! audited doesn't happen.

use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait};
use serde_json::{Map, Value};
use tracing::info;

use crate::persistence::entity::audit_log;

/// What was done. Each flow adds its actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The admin role was given to a user listed in `admin.lineUserIds` (FR-ADM-01)
    AdminGranted,
    /// The admin role was taken from a user no longer listed
    AdminRevoked,
    // Categories (FR-ADM-05)
    CategoryAdded,
    CategoryRenamed,
    /// Moved up or down the order buyers see
    CategoryMoved,
    CategoryDeactivated,
    CategoryActivated,
    CategoryDeleted,
    // Shops (FR-ADM-02)
    /// By an admin, or by the system when `shop.requireApproval` is off (FR-SHOP-03)
    ShopApproved,
    ShopRejected,
}

impl Action {
    /// Its name in `audit_logs.action`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdminGranted => "admin.granted",
            Self::AdminRevoked => "admin.revoked",
            Self::CategoryAdded => "category.added",
            Self::CategoryRenamed => "category.renamed",
            Self::CategoryMoved => "category.moved",
            Self::CategoryDeactivated => "category.deactivated",
            Self::CategoryActivated => "category.activated",
            Self::CategoryDeleted => "category.deleted",
            Self::ShopApproved => "shop.approved",
            Self::ShopRejected => "shop.rejected",
        }
    }
}

/// What it was done to. Each flow adds the kinds of target it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    User(i64),
    Category(i64),
    Shop(i64),
}

impl Target {
    /// Its `audit_logs.target_type` and `target_id`.
    fn parts(self) -> (&'static str, i64) {
        match self {
            Self::User(id) => ("user", id),
            Self::Category(id) => ("category", id),
            Self::Shop(id) => ("shop", id),
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
    /// What else says what was done, such as a category's name before and after a rename, so
    /// the entry still reads once the target has changed or is gone
    pub details: Map<String, Value>,
}

/// The details of an [`Entry`]: `details([("name", json!("ขนม"))])`.
pub fn details<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Writes `entries`, done at `now`, to the audit log. Their details go in `details`, with the
/// reason as `details.reason`, and are not logged: an admin's reason can name people
/// (NFR-PRIV-05).
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
        let mut details = entry.details.clone();
        if let Some(reason) = &entry.reason {
            details.insert("reason".to_owned(), Value::String(reason.clone()));
        }
        audit_log::ActiveModel {
            actor_id: Set(entry.actor_id),
            action: Set(entry.action.as_str().to_owned()),
            target_type: Set(target_type.to_owned()),
            target_id: Set(Some(target_id)),
            details: Set(Value::Object(details)),
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
    use serde_json::json;

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
            details: Map::new(),
        };
        let by_system = Entry {
            actor_id: None,
            action: Action::AdminGranted,
            target: Target::User(target.id),
            reason: None,
            details: Map::new(),
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

    /// The details say what was done, with the reason among them.
    #[tokio::test]
    async fn details_are_kept_with_the_reason() {
        let (_container, pool) = testing::migrated().await;
        let db = orm(pool);
        let admin = user::ActiveModel {
            is_admin: Set(true),
            ..new_user("U-admin")
        }
        .insert(&db)
        .await
        .unwrap();
        let renamed = |reason: Option<&str>| Entry {
            actor_id: Some(admin.id),
            action: Action::CategoryRenamed,
            target: Target::Category(7),
            reason: reason.map(str::to_owned),
            details: details([("from", json!("ขนม")), ("to", json!("ขนม/ของหวาน"))]),
        };
        record(&db, at(), [renamed(None), renamed(Some("สะกดผิด"))])
            .await
            .unwrap();
        let rows: Vec<_> = audited(&db)
            .await
            .into_iter()
            .map(|(_, action, target_type, target_id, details, _)| {
                (action, target_type, target_id, details)
            })
            .collect();
        let row = |details| {
            let (action, target_type) = ("category.renamed".to_owned(), "category".to_owned());
            (action, target_type, Some(7), details)
        };
        assert_eq!(
            rows,
            [
                row(json!({ "from": "ขนม", "to": "ขนม/ของหวาน" })),
                row(json!({ "from": "ขนม", "to": "ขนม/ของหวาน", "reason": "สะกดผิด" })),
            ]
        );
    }
}
