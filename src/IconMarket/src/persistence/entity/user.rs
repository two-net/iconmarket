//! Anyone who has followed the OA (FR-ONB-01).

use sea_orm::entity::prelude::*;

use super::PersonalData;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// `None` once the account is deleted (FR-ONB-07)
    #[sea_orm(unique)]
    pub line_user_id: Option<String>,
    pub display_name: Option<String>,
    pub picture_url: Option<String>,
    /// Synced from `admin.lineUserIds` (FR-ADM-01). The other roles aren't stored: every user
    /// is a buyer, and the owner of an `APPROVED` shop is a seller.
    pub is_admin: bool,
    /// False after `unfollow`; no pushes are sent until they follow again (FR-ONB-06)
    pub is_following: bool,
    pub status: Status,
    /// The privacy notice version consented to (FR-ONB-02)
    pub consent_version: Option<String>,
    pub consented_at: Option<DateTimeUtc>,
    /// The address, captured when first needed (FR-ONB-04)
    pub zone: Option<String>,
    pub house_number: Option<String>,
    pub landmark_note: Option<String>,
    pub phone: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "String(StringLen::None)",
    rename_all = "SCREAMING_SNAKE_CASE"
)]
pub enum Status {
    Active,
    Suspended,
    Deleted,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl PersonalData for Entity {
    const FIELDS: &'static [Column] = &[
        Column::LineUserId,
        Column::DisplayName,
        Column::PictureUrl,
        Column::Zone,
        Column::HouseNumber,
        Column::LandmarkNote,
        Column::Phone,
        Column::Latitude,
        Column::Longitude,
    ];
}
