//! A schema the entity models generated, recorded when auto-migration first applies it
//! (MIG-09).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "schema_versions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// SHA-256 of the DDL the models generate, in hex
    pub snapshot: String,
    /// The application version that applied it
    pub app_version: String,
    pub applied_at: DateTimeUtc,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
