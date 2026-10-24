//! Categories (FR-ADM-05): in the order admins set, which is the order buyers see them in
//! "ซื้อของ" (FR-SRCH-01). Only active ones are offered for browsing, for new listings
//! (FR-PROD-03, #25) and to the AI (`list_categories`, FR-AI-12, #37); admins manage them all
//! ([`admin::categories`](crate::admin::categories)).

use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, Select};

use crate::persistence::entity::category;

/// Every category, in the order admins set. Two can share a place, if added at once while
/// there were none to lock; the older comes first.
pub fn ordered() -> Select<category::Entity> {
    category::Entity::find()
        .order_by_asc(category::Column::SortOrder)
        .order_by_asc(category::Column::Id)
}

/// The active categories, in order: those offered to buyers and sellers.
pub async fn active(db: &impl ConnectionTrait) -> Result<Vec<category::Model>, DbErr> {
    ordered()
        .filter(category::Column::IsActive.eq(true))
        .all(db)
        .await
}
