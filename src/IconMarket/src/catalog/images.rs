//! Images (FR-PROD-04): product and shop photos and payment slips, made safe and stored in
//! PostgreSQL (CON-03), and the two links that serve them (§3.4, NFR-SEC-04).
//!
//! A user's photo is downloaded from LINE while its event is processed (FR-LINE-09), checked
//! and re-encoded ([`process`]), then [`store`]d under a random ID. A product or shop image is
//! public by that ID at `/img/p/{id}`. A slip image is reachable only through its payment slip's
//! [`SlipLink`] at `/img/s/{token}`, until the link expires (AC-14).
//!
//! A photo sent in a guided flow before what it is for exists, such as the photo of a shop
//! applied for or those of a product being added, is a [draft](Owner::Draft) of the user who
//! sent it. The flow [`adopt`]s it for what it makes; one that ends without is swept by
//! [`delete_orphaned_drafts`].

#![allow(
    dead_code,
    reason = "the payment flows that upload slips are built by later issues"
)]

mod process;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::{IntoResponse, Response};
use chrono::{TimeDelta, Utc};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::{DateTimeUtc, Uuid};
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, JoinType, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait, TransactionSession, TransactionTrait,
};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::{debug, error, warn};
use url::Url;

use crate::ai;
use crate::config::ImageConfig;
use crate::http::AppState;
use crate::line;
use crate::persistence::entity::image::{self, OwnerType};
use crate::persistence::entity::{payment_slip, product, session};
use crate::persistence::{self, random_id};

pub use process::{Processed, process};

/// Every stored image is a JPEG.
const JPEG: &str = "image/jpeg";
/// Uploads processed at once; others wait their turn. Decoding a large photo keeps a core busy
/// and takes hundreds of megabytes, so this bounds both on the 2-vCPU server (NFR-CAP-02).
static PROCESSING: Semaphore = Semaphore::const_new(2);
/// An upload is processed and acknowledged within this (NFR-PERF-04)
const TARGET: Duration = Duration::from_secs(5);

/// IDs are random and an image never changes, as a new photo gets a new ID, so caches may keep
/// it for good.
const PUBLIC: &str = "public, max-age=31536000, immutable";
/// A slip shows the payer's name and account, so no cache keeps it past its link's expiry.
const PRIVATE: &str = "private, no-store";

/// How long after their flow was last live a user's drafts are kept. Whether a flow is live is
/// judged when an event starts, and the sweep may run while one is handled, so a draft is kept a
/// while longer than its flow.
const DRAFT_GRACE: TimeDelta = TimeDelta::hours(1);

/// Why an upload was not stored: something the user can fix, each with its guidance
/// ([`Templates::image_rejected`](crate::notifications::templates::Templates::image_rejected)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Larger than `image.maxUploadMB`
    TooLarge { max_mb: u32 },
    /// Not a JPEG or PNG by its content, whatever its name (NFR-SEC-06)
    NotJpegOrPng,
    /// Damaged, or too many pixels to decode safely
    Unreadable,
    /// The product already has `image.maxPerProduct` images
    Full { max: u32 },
    /// LINE no longer has the content, or is still preparing it; sending it again helps
    Unavailable,
}

/// What an image belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Product(i64),
    Shop(i64),
    Slip(i64),
    /// The user with this ID, who sent it in a guided flow (FR-SHOP-01, FR-PROD-01)
    Draft(i64),
}

impl Owner {
    fn parts(self) -> (OwnerType, i64) {
        match self {
            Self::Product(id) => (OwnerType::Product, id),
            Self::Shop(id) => (OwnerType::Shop, id),
            Self::Slip(id) => (OwnerType::Slip, id),
            Self::Draft(user_id) => (OwnerType::Draft, user_id),
        }
    }
}

/// Downloads the image a user sent in message `message_id` (FR-LINE-09) and [`prepare`]s it.
/// A download the user can fix by sending again or sending a smaller photo is a [`Rejected`];
/// any other LINE failure is an error.
pub async fn receive(
    line: &line::Client,
    message_id: &str,
    config: &ImageConfig,
) -> Result<Result<Processed, Rejected>, line::Error> {
    match line.content(message_id, process::max_bytes(config)).await {
        Ok(content) => Ok(prepare(content.bytes, config).await),
        Err(e) => rejected(&e, config).map(Err).ok_or(e),
    }
}

/// The download failures that are the user's to fix.
fn rejected(error: &line::Error, config: &ImageConfig) -> Option<Rejected> {
    match error {
        line::Error::TooLarge { .. } => Some(Rejected::TooLarge {
            max_mb: config.max_upload_mb,
        }),
        // Still preparing, expired, or unsent by the user
        line::Error::NotReady => Some(Rejected::Unavailable),
        line::Error::Api { status, .. }
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE =>
        {
            Some(Rejected::Unavailable)
        }
        _ => None,
    }
}

/// [`process`]es `bytes` on the blocking pool, at most two uploads at a time.
pub async fn prepare(bytes: Vec<u8>, config: &ImageConfig) -> Result<Processed, Rejected> {
    let started = Instant::now();
    let _permit = PROCESSING
        .acquire()
        .await
        .expect("the semaphore is never closed");
    let config = config.clone();
    let result = match tokio::task::spawn_blocking(move || process(&bytes, &config)).await {
        Ok(result) => result,
        // A decoder bug that a crafted file sets off must not take the caller down with it
        Err(e) => {
            error!(error = %e, "image processing failed");
            Err(Rejected::Unreadable)
        }
    };

    let elapsed = started.elapsed();
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    if elapsed > TARGET {
        warn!(
            elapsed_ms,
            "image processing took longer than its 5 s target"
        );
    } else {
        debug!(elapsed_ms, ok = result.is_ok(), "image processed");
    }
    result
}

/// Stores `processed` as `owner`'s last image, at `now`, and returns its new random ID
/// (NFR-SEC-04).
///
/// A product with `image.maxPerProduct` images gets no more. Its row stays locked from the
/// count to the commit, so uploads at the same time can't pass the limit either. A product
/// that doesn't exist is an error.
pub async fn store(
    db: &impl TransactionTrait,
    owner: Owner,
    processed: Processed,
    config: &ImageConfig,
    now: DateTimeUtc,
) -> Result<Result<Uuid, Rejected>, DbErr> {
    let (owner_type, owner_id) = owner.parts();
    let txn = db.begin().await?;
    if let Owner::Product(id) = owner {
        product::Entity::find_by_id(id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("product {id}")))?;
    }
    let positions: Vec<i16> = image::Entity::find()
        .select_only()
        .column(image::Column::Position)
        .filter(image::Column::OwnerType.eq(owner_type))
        .filter(image::Column::OwnerId.eq(owner_id))
        .into_tuple()
        .all(&txn)
        .await?;
    if owner_type == OwnerType::Product
        && positions.len() >= usize::try_from(config.max_per_product).unwrap_or(usize::MAX)
    {
        return Ok(Err(Rejected::Full {
            max: config.max_per_product,
        }));
    }

    let id = random_id();
    let row = image::ActiveModel {
        id: Set(id),
        owner_type: Set(owner_type),
        owner_id: Set(owner_id),
        position: Set(positions
            .iter()
            .max()
            .map_or(0, |last| last.saturating_add(1))),
        content_type: Set(JPEG.to_owned()),
        width: Set(i32::try_from(processed.width).expect("at most 16 384 px")),
        height: Set(i32::try_from(processed.height).expect("at most 16 384 px")),
        size_bytes: Set(i32::try_from(processed.jpeg.len()).expect("far less than 2 GiB")),
        data: Set(processed.jpeg),
        created_at: Set(now),
    };
    image::Entity::insert(row)
        .exec_without_returning(&txn)
        .await?;
    txn.commit().await?;
    Ok(Ok(id))
}

/// Gives user `user_id`'s draft `id` to `owner`, as its first image. Returns whether there was
/// such a draft: it may have been [swept](delete_orphaned_drafts).
pub async fn adopt(
    db: &impl ConnectionTrait,
    id: Uuid,
    user_id: i64,
    owner: Owner,
) -> Result<bool, DbErr> {
    adopt_all(db, &[id], user_id, owner).await
}

/// Gives user `user_id`'s drafts `ids` to `owner`, in that order, such as a product's photos
/// (FR-PROD-01). Returns whether all of them were there: one may have been
/// [swept](delete_orphaned_drafts), and the others are then given all the same, for the caller
/// to roll back.
pub async fn adopt_all(
    db: &impl ConnectionTrait,
    ids: &[Uuid],
    user_id: i64,
    owner: Owner,
) -> Result<bool, DbErr> {
    let (owner_type, owner_id) = owner.parts();
    let mut all = true;
    for (position, &id) in ids.iter().enumerate() {
        let position = i16::try_from(position).expect("a few images");
        let adopted = image::Entity::update_many()
            .col_expr(image::Column::OwnerType, Expr::value(owner_type))
            .col_expr(image::Column::OwnerId, Expr::value(owner_id))
            .col_expr(image::Column::Position, Expr::value(position))
            .filter(image::Column::Id.eq(id))
            .filter(image::Column::OwnerType.eq(OwnerType::Draft))
            .filter(image::Column::OwnerId.eq(user_id))
            .exec(db)
            .await?;
        all &= adopted.rows_affected > 0;
    }
    Ok(all)
}

/// Makes `ids` `owner`'s images, in that order, and deletes its others, such as a product's
/// photos changed in "สินค้าของฉัน" (FR-PROD-05). Each is either `owner`'s already or one of
/// user `user_id`'s drafts, which it adopts. Returns whether all of them were there: a draft may
/// have been [swept](delete_orphaned_drafts), and the others are then given all the same, for the
/// caller to roll back.
pub async fn replace_all(
    db: &impl ConnectionTrait,
    owner: Owner,
    ids: &[Uuid],
    user_id: i64,
) -> Result<bool, DbErr> {
    let (owner_type, owner_id) = owner.parts();
    let mut all = true;
    for (position, &id) in ids.iter().enumerate() {
        let position = i16::try_from(position).expect("a few images");
        let mine = Condition::any()
            .add(
                Condition::all()
                    .add(image::Column::OwnerType.eq(OwnerType::Draft))
                    .add(image::Column::OwnerId.eq(user_id)),
            )
            .add(
                Condition::all()
                    .add(image::Column::OwnerType.eq(owner_type))
                    .add(image::Column::OwnerId.eq(owner_id)),
            );
        let kept = image::Entity::update_many()
            .col_expr(image::Column::OwnerType, Expr::value(owner_type))
            .col_expr(image::Column::OwnerId, Expr::value(owner_id))
            .col_expr(image::Column::Position, Expr::value(position))
            .filter(image::Column::Id.eq(id))
            .filter(mine)
            .exec(db)
            .await?;
        all &= kept.rows_affected > 0;
    }
    image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(owner_type))
        .filter(image::Column::OwnerId.eq(owner_id))
        .filter(image::Column::Id.is_not_in(ids.iter().copied()))
        .exec(db)
        .await?;
    Ok(all)
}

/// Deletes user `user_id`'s draft `id`, replaced by another photo.
pub async fn delete_draft(db: &impl ConnectionTrait, id: Uuid, user_id: i64) -> Result<(), DbErr> {
    image::Entity::delete_many()
        .filter(image::Column::Id.eq(id))
        .filter(image::Column::OwnerType.eq(OwnerType::Draft))
        .filter(image::Column::OwnerId.eq(user_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Deletes `owner`'s images but `keep`: a shop has one photo, and a new one replaces it.
/// Returns how many were deleted.
pub async fn keep_only(
    db: &impl ConnectionTrait,
    owner: Owner,
    keep: Option<Uuid>,
) -> Result<u64, DbErr> {
    let (owner_type, owner_id) = owner.parts();
    let mut others = image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(owner_type))
        .filter(image::Column::OwnerId.eq(owner_id));
    if let Some(keep) = keep {
        others = others.filter(image::Column::Id.ne(keep));
    }
    Ok(others.exec(db).await?.rows_affected)
}

/// The ID of `owner`'s first image, such as a shop's photo.
pub async fn first(db: &impl ConnectionTrait, owner: Owner) -> Result<Option<Uuid>, DbErr> {
    let (owner_type, owner_id) = owner.parts();
    image::Entity::find()
        .select_only()
        .column(image::Column::Id)
        .filter(image::Column::OwnerType.eq(owner_type))
        .filter(image::Column::OwnerId.eq(owner_id))
        .order_by_asc(image::Column::Position)
        .order_by_asc(image::Column::CreatedAt)
        .into_tuple()
        .one(db)
        .await
}

/// The IDs of `owner`'s images, in their order, such as a product's photos.
pub async fn all(db: &impl ConnectionTrait, owner: Owner) -> Result<Vec<Uuid>, DbErr> {
    let (owner_type, owner_id) = owner.parts();
    image::Entity::find()
        .select_only()
        .column(image::Column::Id)
        .filter(image::Column::OwnerType.eq(owner_type))
        .filter(image::Column::OwnerId.eq(owner_id))
        .order_by_asc(image::Column::Position)
        .order_by_asc(image::Column::CreatedAt)
        .into_tuple()
        .all(db)
        .await
}

/// Images `ids` as the AI is sent them, in the order given; one that is gone is left out.
pub async fn media(db: &impl ConnectionTrait, ids: &[Uuid]) -> Result<Vec<ai::Media>, DbErr> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut found: HashMap<Uuid, image::Model> = image::Entity::find()
        .filter(image::Column::Id.is_in(ids.iter().copied()))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    Ok(ids
        .iter()
        .filter_map(|id| found.remove(id))
        .map(|row| ai::Media {
            media_type: row.content_type,
            bytes: row.data,
        })
        .collect())
}

/// The IDs of the images of products `ids`, each product's in their order: what "สินค้าของฉัน"
/// shows of their photos. A product without any has no entry.
pub async fn of_products(
    db: &impl ConnectionTrait,
    ids: &[i64],
) -> Result<HashMap<i64, Vec<Uuid>>, DbErr> {
    let mut photos: HashMap<i64, Vec<Uuid>> = HashMap::new();
    if ids.is_empty() {
        return Ok(photos);
    }
    let rows: Vec<(i64, Uuid)> = image::Entity::find()
        .select_only()
        .column(image::Column::OwnerId)
        .column(image::Column::Id)
        .filter(image::Column::OwnerType.eq(OwnerType::Product))
        .filter(image::Column::OwnerId.is_in(ids.iter().copied()))
        .order_by_asc(image::Column::Position)
        .order_by_asc(image::Column::CreatedAt)
        .into_tuple()
        .all(db)
        .await?;
    for (product_id, id) in rows {
        photos.entry(product_id).or_default().push(id);
    }
    Ok(photos)
}

/// Deletes, at `now`, the drafts of users whose guided flow ended more than [`DRAFT_GRACE`]
/// ago, or who have none: cancelled, expired or finished without adopting them. Returns how
/// many were deleted.
pub async fn delete_orphaned_drafts(
    db: &impl ConnectionTrait,
    now: DateTimeUtc,
) -> Result<u64, DbErr> {
    let live = Query::select()
        .column(session::Column::UserId)
        .from(session::Entity)
        .and_where(session::Column::ExpiresAt.gt(now - DRAFT_GRACE))
        .to_owned();
    let deleted = image::Entity::delete_many()
        .filter(image::Column::OwnerType.eq(OwnerType::Draft))
        .filter(image::Column::OwnerId.not_in_subquery(live))
        .exec(db)
        .await?;
    Ok(deleted.rows_affected)
}

/// The expiring link to a slip image, kept in its payment slip (NFR-SEC-04): a random token
/// used for nothing else, and when it stops working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlipLink {
    pub token: Uuid,
    pub expires_at: DateTimeUtc,
}

impl SlipLink {
    /// A new link made at `now`, working for `image.slipLinkHours` (AC-14).
    pub fn new(now: DateTimeUtc, config: &ImageConfig) -> Self {
        Self {
            token: random_id(),
            expires_at: now + TimeDelta::hours(config.slip_link_hours.into()),
        }
    }
}

/// `/img/p/{id}` under `app.baseUrl`: a product or shop image for Flex and image messages
/// (FR-LINE-01). LINE accepts the same link as the preview, as stored images are small.
pub fn product_image_url(base: &Url, id: Uuid) -> Url {
    link(base, "p", id)
}

/// `/img/s/{token}` under `app.baseUrl`: a slip image, for the seller.
pub fn slip_image_url(base: &Url, token: Uuid) -> Url {
    link(base, "s", token)
}

fn link(base: &Url, kind: &str, id: Uuid) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("an https base URL")
        .pop_if_empty()
        .extend(["img", kind, &id.to_string()]);
    url
}

/// `GET /img/p/{imageId}`: a product or shop image, or a draft of one, public by its random ID.
/// Never a slip image, though it has an ID too.
pub async fn serve_product(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Ok(id) = Uuid::try_parse(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let found = image::Entity::find_by_id(id)
        .filter(image::Column::OwnerType.is_in([
            OwnerType::Product,
            OwnerType::Shop,
            OwnerType::Draft,
        ]))
        .one(&persistence::orm(state.db))
        .await;
    match found {
        Ok(Some(image)) => serve(image, PUBLIC),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => unavailable(&e),
    }
}

/// `GET /img/s/{token}`: a slip image, while its link lasts. An expired link gets the same
/// 404 as one that never existed (AC-14).
pub async fn serve_slip(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Ok(token) = Uuid::try_parse(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let found = image::Entity::find()
        .join_rev(JoinType::InnerJoin, payment_slip::Relation::Image.def())
        .filter(payment_slip::Column::AccessToken.eq(token))
        .filter(payment_slip::Column::TokenExpiresAt.gt(Utc::now()))
        .one(&persistence::orm(state.db))
        .await;
    match found {
        Ok(Some(image)) => serve(image, PRIVATE),
        // Unknown, expired, or its image deleted by retention
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => unavailable(&e),
    }
}

fn serve(image: image::Model, cache: &'static str) -> Response {
    (
        [(CONTENT_TYPE, image.content_type)],
        [(CACHE_CONTROL, cache), (X_CONTENT_TYPE_OPTIONS, "nosniff")],
        image.data,
    )
        .into_response()
}

fn unavailable(e: &DbErr) -> Response {
    error!(error = %e, "cannot read an image from the database");
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}
