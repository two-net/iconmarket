//! An admin's announcement to every follower, "ประกาศ" (FR-ADM-07). Written in a flow and kept
//! unsent until the admin confirms it, so that its photo, an image it owns, is safe from the
//! draft sweep meanwhile. One never confirmed is deleted once its confirm button expires, or as
//! soon as its admin starts writing another.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "announcements")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Who wrote it and sends it
    #[sea_orm(indexed)]
    pub admin_id: i64,
    /// As the admin typed it, without the heading recipients see above it
    pub text: String,
    pub created_at: DateTimeUtc,
    /// `None` until the admin confirms it
    #[sea_orm(indexed)]
    pub sent_at: Option<DateTimeUtc>,
    /// The followers it was queued for, once sent
    pub recipients: Option<i32>,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::AdminId",
        to = "super::user::Column::Id"
    )]
    Admin,
}

impl ActiveModelBehavior for ActiveModel {}
