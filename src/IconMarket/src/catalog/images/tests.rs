//! Uploads checked by size and content, re-encoded without metadata and shrunk (FR-PROD-04,
//! NFR-SEC-06); stored with the per-product limit; and served by random ID or by expiring slip
//! link (§3.4, AC-14). The database tests each start a throwaway PostgreSQL, so they need
//! Docker.

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, header};
use chrono::Utc;
// `::image` is the crate; `image` is the entity
use ::image::codecs::jpeg::JpegEncoder;
use ::image::{DynamicImage, GrayImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use sea_orm::{ActiveModelTrait, DatabaseConnection, IntoActiveModel, QueryOrder};
use tokio::task::JoinSet;
use tower::ServiceExt;

use super::*;
use crate::config::Secret;
use crate::persistence::testing::{self, market, new_order};
use crate::persistence::{entity::order, orm};
use crate::thai::Money;
use crate::{dispatcher, http, payments, webhook};

fn config() -> ImageConfig {
    ImageConfig {
        max_upload_mb: 1,
        max_dimension_px: 100,
        max_per_product: 3,
        slip_link_hours: 24,
    }
}

/// A photo-like picture: a smooth gradient, so the JPEG has real content.
fn picture(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        let shade = |at: u32, of: u32| u8::try_from(at * 255 / of).unwrap();
        Rgb([shade(x, width), shade(y, height), 128])
    })
}

fn jpeg(picture: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new(&mut out).encode_image(picture).unwrap();
    out
}

fn png(image: impl Into<DynamicImage>) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.into().write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

/// `jpeg` with an Exif segment, as a phone writes one: an orientation, and the GPS position
/// it was taken at (13° 45′ N).
fn with_exif(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    // Big-endian TIFF: IFD0 at 8, the GPS IFD at 38 and its latitude at 68
    let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08".to_vec();
    tiff.extend(2u16.to_be_bytes());
    // Orientation: SHORT, one, left-justified
    tiff.extend([0x01, 0x12, 0, 3, 0, 0, 0, 1]);
    tiff.extend(orientation.to_be_bytes());
    tiff.extend([0, 0]);
    // GPSInfo: LONG, one, the GPS IFD's offset
    tiff.extend([0x88, 0x25, 0, 4, 0, 0, 0, 1]);
    tiff.extend(38u32.to_be_bytes());
    tiff.extend(0u32.to_be_bytes());
    // The GPS IFD: GPSLatitudeRef "N", and GPSLatitude as three RATIONALs at 68
    tiff.extend(2u16.to_be_bytes());
    tiff.extend([0x00, 0x01, 0, 2, 0, 0, 0, 2, b'N', 0, 0, 0]);
    tiff.extend([0x00, 0x02, 0, 5, 0, 0, 0, 3]);
    tiff.extend(68u32.to_be_bytes());
    tiff.extend(0u32.to_be_bytes());
    assert_eq!(tiff.len(), 68);
    for (numerator, denominator) in [(13u32, 1u32), (45, 1), (0, 1)] {
        tiff.extend(numerator.to_be_bytes());
        tiff.extend(denominator.to_be_bytes());
    }

    let mut segment = vec![0xFF, 0xE1];
    segment.extend(u16::try_from(2 + 6 + tiff.len()).unwrap().to_be_bytes());
    segment.extend(b"Exif\x00\x00");
    segment.extend(tiff);
    // Right after the start-of-image marker
    [&jpeg[..2], &segment, &jpeg[2..]].concat()
}

/// The markers of `jpeg`'s segments up to its image data: everything a reader of its metadata
/// sees.
fn markers(jpeg: &[u8]) -> Vec<u8> {
    assert_eq!(jpeg[..2], [0xFF, 0xD8], "starts with SOI");
    let mut markers = Vec::new();
    let mut at = 2;
    loop {
        assert_eq!(jpeg[at], 0xFF, "a marker at {at}");
        let marker = jpeg[at + 1];
        markers.push(marker);
        // Start of scan: entropy-coded data follows
        if marker == 0xDA {
            return markers;
        }
        at += 2 + usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
    }
}

/// Fails unless `jpeg` holds the image and its JFIF header only: no Exif, XMP or ICC (APP1 to
/// APP15) and no comment.
#[track_caller]
fn assert_no_metadata(jpeg: &[u8]) {
    let metadata: Vec<_> = markers(jpeg)
        .into_iter()
        .filter(|marker| matches!(marker, 0xE1..=0xEF | 0xFE))
        .collect();
    assert!(metadata.is_empty(), "metadata segments {metadata:02X?}");
    assert!(!jpeg.windows(4).any(|bytes| bytes == b"Exif"));
    assert_eq!(jpeg[jpeg.len() - 2..], [0xFF, 0xD9], "nothing after EOI");
}

fn decoded(processed: &Processed) -> RgbImage {
    let image = ::image::load_from_memory_with_format(&processed.jpeg, ImageFormat::Jpeg).unwrap();
    assert_eq!(
        (image.width(), image.height()),
        (processed.width, processed.height)
    );
    image.into_rgb8()
}

/// Acceptance (#11): what counts is the content, so text in a file named `.jpg` is refused,
/// as are images in other formats.
#[test]
fn a_non_image_named_jpg_is_rejected() {
    for bytes in [
        b"just text, though the file was called photo.jpg".to_vec(),
        b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00!".to_vec(),
        b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec(),
        Vec::new(),
    ] {
        assert_eq!(process(&bytes, &config()), Err(Rejected::NotJpegOrPng));
    }
}

#[test]
fn uploads_over_the_limit_are_refused_before_decoding() {
    let mut bytes = jpeg(&picture(8, 8));
    bytes.resize(1024 * 1024 + 1, 0);
    assert_eq!(
        process(&bytes, &config()),
        Err(Rejected::TooLarge { max_mb: 1 })
    );

    let mut bytes = jpeg(&picture(8, 8));
    bytes.resize(1024 * 1024, 0);
    assert!(
        process(&bytes, &config()).is_ok(),
        "exactly the limit is fine"
    );
}

#[test]
fn damaged_images_are_unreadable() {
    let jpeg = jpeg(&picture(40, 30));
    let png = png(picture(40, 30));
    for damaged in [&jpeg[..20], &png[..30]] {
        assert_eq!(process(damaged, &config()), Err(Rejected::Unreadable));
    }
}

/// A small file that would decode into more pixels than is safe is refused unopened.
#[test]
fn pictures_too_large_to_decode_safely_are_unreadable() {
    let wide = png(GrayImage::new(16_385, 1));
    assert!(wide.len() < 1024 * 1024);
    assert_eq!(process(&wide, &config()), Err(Rejected::Unreadable));

    let widest_allowed = png(GrayImage::new(16_384, 1));
    let processed = process(&widest_allowed, &config()).unwrap();
    assert_eq!((processed.width, processed.height), (100, 1));
}

/// Acceptance (#11): no Exif or GPS location is kept (FR-PROD-04).
#[test]
fn metadata_including_gps_is_removed() {
    let upload = with_exif(&jpeg(&picture(40, 30)), 1);
    assert!(markers(&upload).contains(&0xE1), "the upload has Exif");

    let processed = process(&upload, &config()).unwrap();
    assert_no_metadata(&processed.jpeg);
    assert_eq!(markers(&processed.jpeg)[0], 0xE0, "a JFIF header");
}

/// The Exif orientation is applied before the Exif is dropped, so photos stay upright.
#[test]
fn photos_are_turned_upright() {
    let upload = jpeg(&picture(40, 20));
    let processed = process(&with_exif(&upload, 1), &config()).unwrap();
    assert_eq!((processed.width, processed.height), (40, 20));

    // 6: the camera was turned a quarter clockwise, so the picture is turned back
    let processed = process(&with_exif(&upload, 6), &config()).unwrap();
    assert_eq!((processed.width, processed.height), (20, 40));
    let upright = decoded(&processed);
    // The source's top-left corner, dark in both channels, ends up top-right
    let top_right = upright.get_pixel(19, 0);
    assert!(top_right[0] < 40 && top_right[1] < 40, "{top_right:?}");
}

#[test]
fn the_longest_side_is_shrunk_to_the_maximum() {
    for ((width, height), expected) in [
        ((300, 150), (100, 50)),
        ((150, 300), (50, 100)),
        ((333, 100), (100, 30)),
        ((100, 100), (100, 100)),
        // Never enlarged
        ((60, 40), (60, 40)),
    ] {
        let processed = process(&jpeg(&picture(width, height)), &config()).unwrap();
        assert_eq!(
            (processed.width, processed.height),
            expected,
            "{width}×{height}"
        );
        decoded(&processed);
    }
}

#[test]
fn png_becomes_jpeg_with_transparency_on_white() {
    let mut picture = RgbaImage::from_pixel(20, 20, Rgba([0, 0, 0, 0]));
    picture.put_pixel(10, 10, Rgba([255, 0, 0, 255]));
    let processed = process(&png(picture), &config()).unwrap();
    assert_eq!(
        ::image::guess_format(&processed.jpeg).unwrap(),
        ImageFormat::Jpeg
    );
    assert_no_metadata(&processed.jpeg);

    let corner = *decoded(&processed).get_pixel(0, 0);
    assert!(corner.0.iter().all(|&channel| channel > 245), "{corner:?}");
}

#[test]
fn download_failures_the_user_can_fix_are_rejections() {
    let api = |status| line::Error::Api {
        status,
        message: String::new(),
        details: Vec::new(),
        request_id: None,
    };
    let config = config();
    assert_eq!(
        rejected(&line::Error::TooLarge { max_bytes: 1 }, &config),
        Some(Rejected::TooLarge { max_mb: 1 })
    );
    assert_eq!(
        rejected(&line::Error::NotReady, &config),
        Some(Rejected::Unavailable)
    );
    assert_eq!(
        rejected(&api(StatusCode::NOT_FOUND), &config),
        Some(Rejected::Unavailable)
    );
    assert_eq!(
        rejected(&api(StatusCode::GONE), &config),
        Some(Rejected::Unavailable)
    );
    assert_eq!(
        rejected(&api(StatusCode::INTERNAL_SERVER_ERROR), &config),
        None
    );
    assert_eq!(rejected(&api(StatusCode::TOO_MANY_REQUESTS), &config), None);
}

#[test]
fn links_are_built_under_the_base_url() {
    let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    for base in ["https://market.example.com", "https://market.example.com/"] {
        assert_eq!(
            product_image_url(&Url::parse(base).unwrap(), id).as_str(),
            "https://market.example.com/img/p/01234567-89ab-cdef-0123-456789abcdef"
        );
    }
    assert_eq!(
        slip_image_url(&Url::parse("https://example.com/market/").unwrap(), id).as_str(),
        "https://example.com/market/img/s/01234567-89ab-cdef-0123-456789abcdef"
    );
    assert_eq!(
        placeholder_url(&Url::parse("https://example.com/market/").unwrap()).as_str(),
        "https://example.com/market/img/placeholder.png"
    );
}

/// FR-SRCH-07: a product card without a photo shows the placeholder, a PNG as large as the cards
/// show photos, 20:13, served without the database.
#[tokio::test]
async fn the_placeholder_is_served_as_cards_show_photos() {
    let db = orm(testing::unreachable());
    let response = get(&db, "/img/placeholder.png").await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::CACHE_CONTROL], PLACEHOLDER_CACHE);
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    let png = body(response).await;
    let placeholder = ::image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
    let (width, height) = (placeholder.width(), placeholder.height());
    assert_eq!(width * 13, height * 20, "{width}×{height}");
}

#[test]
fn a_slip_link_lasts_slip_link_hours() {
    let now = Utc::now();
    let link = SlipLink::new(now, &config());
    assert_eq!(link.expires_at, now + TimeDelta::hours(24));
    assert_ne!(link.token, SlipLink::new(now, &config()).token);
}

/// The app over `db`, as the server runs it.
fn app(db: &DatabaseConnection) -> axum::Router {
    http::router(AppState {
        db: db.get_postgres_connection_pool().clone(),
        webhook: webhook::Verifier::new(&Secret::new("channel-secret")),
        wakeup: dispatcher::Wakeup::default(),
    })
}

async fn get(db: &DatabaseConnection, uri: &str) -> Response {
    let request = Request::get(uri).body(Body::empty()).unwrap();
    app(db).oneshot(request).await.unwrap()
}

async fn body(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn photo() -> Processed {
    process(&jpeg(&picture(40, 30)), &config()).unwrap()
}

/// Acceptance (#11): the stored bytes carry no Exif or GPS location; and the image is public
/// by its random ID, with nothing else served there.
#[tokio::test]
async fn a_stored_photo_has_no_metadata_and_is_served_by_its_random_id() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;

    let upload = with_exif(&jpeg(&picture(40, 30)), 1);
    let processed = prepare(upload, &config()).await.unwrap();
    let product = Owner::Product(market.product.id);
    let id = store(&db, product, processed.clone(), &config(), Utc::now())
        .await
        .unwrap()
        .unwrap();

    let stored = image::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_no_metadata(&stored.data);
    assert_eq!(stored.data, processed.jpeg);
    assert_eq!(
        (stored.content_type.as_str(), stored.width, stored.height),
        ("image/jpeg", 40, 30)
    );
    assert_eq!(
        stored.size_bytes,
        i32::try_from(processed.jpeg.len()).unwrap()
    );

    let response = get(&db, &format!("/img/p/{id}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(headers[header::CACHE_CONTROL], PUBLIC);
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(body(response).await, processed.jpeg);

    let shop = store(
        &db,
        Owner::Shop(market.shop.id),
        photo(),
        &config(),
        Utc::now(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        get(&db, &format!("/img/p/{shop}")).await.status(),
        StatusCode::OK
    );
    let unknown = random_id();
    assert_eq!(
        get(&db, &format!("/img/p/{unknown}")).await.status(),
        StatusCode::NOT_FOUND
    );
}

/// AC-14: a slip-image link older than `image.slipLinkHours` is denied. A slip image is never
/// public by its ID.
#[tokio::test]
async fn a_slip_link_is_denied_after_slip_link_hours() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;
    let order = new_order(market.buyer.id, market.shop.id, "20261024-001", random_id())
        .insert(&db)
        .await
        .unwrap();

    let now = Utc::now();
    let fresh = slip(&db, &order, SlipLink::new(now, &config())).await;
    let made_too_long_ago = now - TimeDelta::hours(24) - TimeDelta::seconds(1);
    let expired = slip(&db, &order, SlipLink::new(made_too_long_ago, &config())).await;

    let response = get(&db, &format!("/img/s/{}", fresh.access_token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(headers[header::CACHE_CONTROL], PRIVATE);
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(body(response).await, photo().jpeg);

    for token in [expired.access_token, random_id()] {
        let status = get(&db, &format!("/img/s/{token}")).await.status();
        assert_eq!(status, StatusCode::NOT_FOUND, "{token}");
    }
    for slip in [fresh, expired] {
        let image_id = slip.image_id.unwrap();
        let status = get(&db, &format!("/img/p/{image_id}")).await.status();
        assert_eq!(status, StatusCode::NOT_FOUND, "slip image {image_id}");
    }
}

/// FR-PAY-04: an order's PromptPay QR code is a PNG served by its random ID, which no cache may
/// keep, as it shows the shop's PromptPay ID.
#[tokio::test]
async fn a_promptpay_qr_code_is_served_by_its_random_id_and_never_cached() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;
    let order = new_order(market.buyer.id, market.shop.id, "20261024-001", random_id())
        .insert(&db)
        .await
        .unwrap();

    let payload = payments::promptpay_payload("0812345678", Some(Money::from_baht(230))).unwrap();
    let qr = payments::draw_qr(&payload);
    let owner = Owner::PaymentQr(order.id);
    let id = store_png(&db, owner, qr.png.clone(), qr.side, qr.side, Utc::now())
        .await
        .unwrap();

    let stored = image::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let side = i32::try_from(qr.side).unwrap();
    assert_eq!(
        (stored.owner_type, stored.owner_id, stored.position),
        (OwnerType::PaymentQr, order.id, 0)
    );
    assert_eq!(
        (stored.content_type.as_str(), stored.width, stored.height),
        ("image/png", side, side)
    );
    assert_eq!(stored.size_bytes, i32::try_from(qr.png.len()).unwrap());

    let response = get(&db, &format!("/img/p/{id}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::CACHE_CONTROL], PRIVATE);
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(payments::scan_qr(&body(response).await), payload);
}

/// A payment slip for `order` with a stored image, reachable through `link`, as the payment
/// flow makes one.
async fn slip(
    db: &DatabaseConnection,
    order: &order::Model,
    link: SlipLink,
) -> payment_slip::Model {
    let slip = payment_slip::ActiveModel {
        order_id: Set(order.id),
        access_token: Set(link.token),
        token_expires_at: Set(link.expires_at),
        status: Set(payment_slip::Status::Submitted),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let image_id = store(db, Owner::Slip(slip.id), photo(), &config(), Utc::now())
        .await
        .unwrap()
        .unwrap();
    let mut slip = slip.into_active_model();
    slip.image_id = Set(Some(image_id));
    slip.update(db).await.unwrap()
}

async fn positions(db: &DatabaseConnection, product_id: i64) -> Vec<i16> {
    image::Entity::find()
        .select_only()
        .column(image::Column::Position)
        .filter(image::Column::OwnerType.eq(OwnerType::Product))
        .filter(image::Column::OwnerId.eq(product_id))
        .order_by_asc(image::Column::Position)
        .into_tuple()
        .all(db)
        .await
        .unwrap()
}

/// FR-PROD-04: at most `image.maxPerProduct` images per product, in the order they came.
#[tokio::test]
async fn a_product_has_at_most_max_per_product_images() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;
    let product = Owner::Product(market.product.id);

    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = store(&db, product, photo(), &config(), Utc::now()).await;
        ids.push(id.unwrap().unwrap());
    }
    let fourth = store(&db, product, photo(), &config(), Utc::now()).await;
    assert_eq!(fourth.unwrap(), Err(Rejected::Full { max: 3 }));
    assert_eq!(positions(&db, market.product.id).await, [0, 1, 2]);
    // Random, not one after another (NFR-SEC-04)
    assert!(ids.windows(2).all(|pair| pair[0] != pair[1]));

    // The limit is per product, and only for products
    for owner in [Owner::Shop(market.shop.id), Owner::Slip(1)] {
        for _ in 0..4 {
            let stored = store(&db, owner, photo(), &config(), Utc::now()).await;
            assert!(stored.unwrap().is_ok(), "{owner:?}");
        }
    }

    let missing = store(&db, Owner::Product(-1), photo(), &config(), Utc::now()).await;
    assert!(
        matches!(missing, Err(DbErr::RecordNotFound(_))),
        "{missing:?}"
    );
}

#[tokio::test]
async fn uploads_at_the_same_time_cannot_pass_the_limit() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let product = Owner::Product(market(&db).await.product.id);

    let mut uploads = JoinSet::new();
    for _ in 0..5 {
        let db = db.clone();
        uploads.spawn(async move { store(&db, product, photo(), &config(), Utc::now()).await });
    }
    let results = uploads.join_all().await;
    let stored = results
        .iter()
        .filter(|result| matches!(result, Ok(Ok(_))))
        .count();
    let full = results
        .iter()
        .filter(|result| matches!(result, Ok(Err(Rejected::Full { max: 3 }))))
        .count();
    assert_eq!((stored, full), (3, 2), "{results:?}");
    let Owner::Product(product_id) = product else {
        unreachable!()
    };
    assert_eq!(positions(&db, product_id).await, [0, 1, 2]);
}

/// FR-SHOP-01: a photo sent before its shop exists is the sender's draft, public like the
/// photo it will be; the shop adopts it once made, as its one photo, and only its sender's
/// draft can be adopted, once.
#[tokio::test]
async fn a_draft_is_adopted_by_what_it_is_for() {
    let (_container, pool) = testing::migrated().await;
    let db = orm(pool);
    let market = market(&db).await;
    let (sender, other) = (market.shop.owner_id, market.buyer.id);
    let shop = Owner::Shop(market.shop.id);

    let draft = store(&db, Owner::Draft(sender), photo(), &config(), Utc::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        get(&db, &format!("/img/p/{draft}")).await.status(),
        StatusCode::OK
    );
    let old = store(&db, shop, photo(), &config(), Utc::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first(&db, shop).await.unwrap(), Some(old));

    assert!(!adopt(&db, draft, other, shop).await.unwrap());
    assert!(adopt(&db, draft, sender, shop).await.unwrap());
    assert!(!adopt(&db, draft, sender, shop).await.unwrap());
    let adopted = image::Entity::find_by_id(draft)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (adopted.owner_type, adopted.owner_id, adopted.position),
        (OwnerType::Shop, market.shop.id, 0)
    );

    assert_eq!(keep_only(&db, shop, Some(draft)).await.unwrap(), 1);
    assert_eq!(first(&db, shop).await.unwrap(), Some(draft));
    assert_eq!(keep_only(&db, shop, None).await.unwrap(), 1);
    assert_eq!(first(&db, shop).await.unwrap(), None);

    // A replaced draft goes, and only its sender's
    let replaced = store(&db, Owner::Draft(sender), photo(), &config(), Utc::now())
        .await
        .unwrap()
        .unwrap();
    delete_draft(&db, replaced, other).await.unwrap();
    assert_eq!(
        first(&db, Owner::Draft(sender)).await.unwrap(),
        Some(replaced)
    );
    delete_draft(&db, replaced, sender).await.unwrap();
    assert_eq!(first(&db, Owner::Draft(sender)).await.unwrap(), None);
}
