//! Fills a migrated database with the target scale, and with the history a service at that scale
//! has built up by the time retention bounds it. Row counts are real. Only a share of the images
//! hold photo-sized data, because a laptop can't spare the disk; `size` projects the rest.
//!
//! The first 20 shops sell nothing, so their owners can list products with no order interrupting
//! the flow. A product's shop and age are hashed, not counted, because a category shows its
//! newest products first, and those shouldn't all be a few shops'. Everything old is in a final
//! status, so no job acts on it during a run.

use std::time::Instant;

use sqlx::{AssertSqlSafe, PgPool};

use crate::{Options, jpeg};

const WORDS: &str =
    "ARRAY['ไข่ไก่','ขนมครก','ผักบุ้ง','หมูปิ้ง','ข้าวเหนียว','น้ำพริก','กล้วยทอด','ส้มตำ','ปลาทู','กาแฟ']";
const ABOUT: &str = "ทำสดใหม่ทุกวัน วัตถุดิบคัดอย่างดีจากในหมู่บ้าน สั่งล่วงหน้าได้ ";

/// Shops whose owners only list products; see the module docs.
pub const LISTING_SHOPS: u32 = 20;

/// The LINE user ID of the `n`th seeded user, from 1. The first `shops` own the shops.
pub fn line_id(n: usize) -> String {
    format!("U{n:032x}")
}

async fn step(db: &PgPool, what: &str, sql: &str) -> anyhow::Result<()> {
    let started = Instant::now();
    let done = sqlx::raw_sql(AssertSqlSafe(sql.to_owned()))
        .execute(db)
        .await?;
    println!(
        "{what}: {} rows in {:.1} s",
        done.rows_affected(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn images(
    db: &PgPool,
    what: &str,
    select: &str,
    real: &[u8],
    stand_in: &[u8],
) -> anyhow::Result<()> {
    let started = Instant::now();
    let sql = format!(
        "INSERT INTO images (id, owner_type, owner_id, position, content_type, width, height,
                             size_bytes, data, created_at)
         SELECT id, owner_type, owner_id, position, 'image/jpeg', 1024, 768,
                CASE WHEN is_real THEN octet_length($1::bytea) ELSE octet_length($2::bytea) END,
                CASE WHEN is_real THEN $1::bytea ELSE $2::bytea END, now()
         FROM ({select}) picked"
    );
    let done = sqlx::query(AssertSqlSafe(sql))
        .bind(real)
        .bind(stand_in)
        .execute(db)
        .await?;
    println!(
        "{what}: {} rows in {:.1} s",
        done.rows_affected(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

pub async fn seed(options: &Options) -> anyhow::Result<()> {
    let users: u32 = options.number("users", 3_000)?;
    let shops: u32 = options.number("shops", 300)?;
    let products: u32 = options.number("products", 10_000)?;
    let order_days: u32 = options.number("order-days", 365)?;
    let event_days: u32 = options.number("event-days", 15)?;
    let events_per_day: u32 = options.number("events-per-day", 40_000)?;
    let real_percent: u32 = options.number("real-image-percent", 10)?;
    let photo_kb: usize = options.number("photo-kb", 150)?;
    let db = options.database("db", 2).await?;

    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM shops")
        .fetch_one(&db)
        .await?;
    anyhow::ensure!(
        existing == 0,
        "the database already has shops: seed an empty one"
    );

    let line_user = "'U' || lpad(to_hex(g), 32, '0')";
    step(
        &db,
        "users",
        &format!(
            "INSERT INTO users (line_user_id, display_name, profile_refreshed_at, is_admin,
                                menu_roles, is_following, status, consent_version, consented_at,
                                zone, house_number, created_at, updated_at)
             SELECT {line_user}, 'ผู้ใช้ ' || g,
                    now() - CASE WHEN g % 10 < 3 THEN interval '2 days' ELSE interval '0' END,
                    false, CASE WHEN g <= {shops} THEN 'SELLER' END, true, 'ACTIVE', '1', now(),
                    'ซอย ' || (g % 12 + 1), (g % 200 + 1)::text, now(), now()
             FROM generate_series(1, {users}) g"
        ),
    )
    .await?;
    step(
        &db,
        "shops",
        &format!(
            "INSERT INTO shops (owner_id, name, description, status, is_open, offers_pickup,
                                offers_delivery, delivery_fee, promptpay_id, phone, show_phone,
                                created_at, updated_at)
             SELECT u.id, 'ร้าน' || ({WORDS})[1 + g % 10] || ' ' || g, '{ABOUT}', 'APPROVED',
                    g % 10 <> 0, true, g % 4 <> 0, 10.00,
                    CASE WHEN g % 5 <> 0 THEN '0812345678' END, '0812345678', true, now(), now()
             FROM generate_series(1, {shops}) g
             JOIN users u ON u.line_user_id = {line_user}"
        ),
    )
    .await?;
    let selling = shops - LISTING_SHOPS;
    step(
        &db,
        "products",
        &format!(
            "INSERT INTO products (shop_id, category_id, name, description, price, unit, stock,
                                   status, created_at, updated_at)
             SELECT s.id, c.id, ({WORDS})[1 + g % 10] || ' สูตร ' || g, repeat('{ABOUT}', 4),
                    20 + g % 200, 'ชิ้น', CASE WHEN g % 3 = 0 THEN NULL ELSE 1000000 END, 'ACTIVE',
                    now() - (abs(hashint4(g)) % 720) * interval '1 hour',
                    now() - (abs(hashint4(g)) % 720) * interval '1 hour'
             FROM generate_series(1, {products}) g
             JOIN (SELECT id, row_number() OVER (ORDER BY id) n FROM shops) s
               ON s.n = {LISTING_SHOPS} + 1 + abs(hashint4(g + 1)) % {selling}
             JOIN (SELECT id, row_number() OVER (ORDER BY id) n FROM categories) c
               ON c.n = 1 + g % (SELECT count(*) FROM categories)"
        ),
    )
    .await?;

    // Only ever served, never decoded, so its size matters and its end doesn't
    let real = jpeg(1024, 768, 40, 85)?;
    let stand_in = vec![0_u8; 1024];
    println!(
        "photo: {} KB generated, {photo_kb} KB wanted",
        real.len() / 1024
    );
    let real = real.get(..photo_kb * 1024).unwrap_or(&real);
    images(
        &db,
        "product images",
        &format!(
            "SELECT gen_random_uuid() id, 'PRODUCT' owner_type, p.id owner_id,
                    position::smallint position, (p.id + position) % 100 < {real_percent} is_real
             FROM products p CROSS JOIN generate_series(0, 1) position"
        ),
        real,
        &stand_in,
    )
    .await?;

    let orders = order_days * 1_000;
    step(
        &db,
        "orders",
        &format!(
            "WITH buyers AS (SELECT array_agg(id) ids FROM users WHERE menu_roles IS NULL AND status = 'ACTIVE'),
                  sellers AS (SELECT array_agg(id) ids FROM shops)
             INSERT INTO orders (order_number, checkout_key, buyer_id, shop_id, status, fulfillment,
                                 delivery_zone, delivery_house_number, subtotal, delivery_fee,
                                 total, payment_method, payment_status, created_at, updated_at,
                                 accepted_at, completed_at)
             SELECT 'IM-' || to_char(at_day, 'YYYYMMDD') || '-' || lpad((g % 1000 + 1)::text, 4, '0'),
                    gen_random_uuid(), buyers.ids[1 + g % array_length(buyers.ids, 1)],
                    sellers.ids[1 + g % array_length(sellers.ids, 1)], 'COMPLETED', 'DELIVERY',
                    'ซอย 3', '45/2', 120, 10, 130,
                    CASE WHEN g % 5 < 2 THEN 'PROMPTPAY' ELSE 'CASH' END, 'PAID', at_day, at_day,
                    at_day, at_day
             FROM generate_series(0, {orders} - 1) g
             CROSS JOIN buyers CROSS JOIN sellers
             CROSS JOIN LATERAL (SELECT now() - (1 + g / 1000) * interval '1 day' at_day) t"
        ),
    )
    .await?;
    step(
        &db,
        "order items",
        "INSERT INTO order_items (order_id, product_id, name, unit, unit_price, quantity, line_total)
         SELECT o.id, p.id, p.name, p.unit, 60, line, 60 * line
         FROM orders o
         CROSS JOIN generate_series(1, 2) line
         JOIN (SELECT id, name, unit, row_number() OVER (ORDER BY id) n FROM products) p
           ON p.n = 1 + (o.id + line) % (SELECT count(*) FROM products)
         WHERE line = 1 OR o.id % 2 = 0",
    )
    .await?;
    step(
        &db,
        "order events",
        "INSERT INTO order_events (order_id, from_status, to_status, actor_type, actor_id, created_at)
         SELECT o.id, moved.from_status, moved.to_status, moved.actor, NULL, o.created_at
         FROM orders o
         CROSS JOIN (VALUES (NULL, 'PENDING', 'BUYER'), ('PENDING', 'ACCEPTED', 'SELLER'),
                            ('ACCEPTED', 'DELIVERING', 'SELLER'), ('DELIVERING', 'COMPLETED', 'SELLER'))
              AS moved(from_status, to_status, actor)",
    )
    .await?;

    images(
        &db,
        "slip images",
        &format!(
            "SELECT id, 'SLIP' owner_type, slip_id owner_id, 0::smallint position, is_real
             FROM (SELECT gen_random_uuid() id, slip.id slip_id, slip.id % 100 < {real_percent} is_real
                   FROM (SELECT id FROM orders
                         WHERE payment_method = 'PROMPTPAY' AND created_at > now() - interval '90 days') slip) picked"
        ),
        real,
        &stand_in,
    )
    .await?;
    step(
        &db,
        "payment slips",
        "INSERT INTO payment_slips (order_id, image_id, access_token, token_expires_at, status, created_at)
         SELECT i.owner_id, i.id, gen_random_uuid(), now(), 'CONFIRMED', now()
         FROM images i WHERE i.owner_type = 'SLIP'",
    )
    .await?;
    step(
        &db,
        "slip owners",
        "UPDATE images i SET owner_id = s.id
         FROM payment_slips s WHERE s.image_id = i.id AND i.owner_type = 'SLIP'",
    )
    .await?;

    let events = event_days * events_per_day;
    step(
        &db,
        "webhook events",
        &format!(
            "INSERT INTO webhook_events (event_id, user_key, event_type, payload, received_at,
                                         status, attempts, processed_at)
             SELECT 'seed-' || g, u.line_user_id, kind, jsonb_build_object(
                        'type', kind, 'mode', 'active', 'timestamp', 1761300000000 + g,
                        'webhookEventId', 'seed-' || g, 'replyToken', md5(g::text),
                        'deliveryContext', jsonb_build_object('isRedelivery', false),
                        'source', jsonb_build_object('type', 'user', 'userId', u.line_user_id),
                        'message', jsonb_build_object('id', g::text, 'type', 'text', 'text', 'เมนู')),
                    at_time, 'DONE', 1, at_time
             FROM generate_series(1, {events}) g
             JOIN users u ON u.line_user_id = 'U' || lpad(to_hex(1 + g % {users}), 32, '0')
             CROSS JOIN LATERAL (SELECT
                 CASE WHEN g % 2 = 0 THEN 'postback' ELSE 'message' END kind,
                 now() - interval '1 hour' - (g::float8 / {events} * {event_days}) * interval '1 day' at_time) t"
        ),
    )
    .await?;
    step(
        &db,
        "push logs",
        &format!(
            "INSERT INTO push_logs (push_type, recipient, retry_key, status, sent_at, attempts, queued_at)
             SELECT 'new_order', 'U' || lpad(to_hex(1 + g % {users}), 32, '0'), gen_random_uuid(),
                    'SENT', at_time, 1, at_time
             FROM generate_series(1, 270000) g
             CROSS JOIN LATERAL (SELECT now() - (g::float8 / 3000) * interval '1 day' at_time) t"
        ),
    )
    .await?;
    step(
        &db,
        "audit logs",
        "INSERT INTO audit_logs (actor_id, action, target_type, target_id, details, created_at)
         SELECT NULL, 'shop.approved', 'shop', g, '{}'::jsonb,
                now() - (g % 365) * interval '1 day'
         FROM generate_series(1, 5000) g",
    )
    .await?;
    step(
        &db,
        "AI call logs",
        "INSERT INTO ai_call_logs (user_id, feature, provider, model, input_tokens, output_tokens,
                                   latency_ms, outcome, created_at)
         SELECT NULL, 'assistant', 'claude', 'claude-opus-5', 1200, 200, 2500, 'ok',
                now() - (g::float8 / 2000) * interval '1 day'
         FROM generate_series(1, 180000) g",
    )
    .await?;
    step(
        &db,
        "AI messages",
        &format!(
            "INSERT INTO ai_messages (user_id, role, content, created_at)
             SELECT (SELECT min(id) FROM users) + g % {users},
                    CASE WHEN g % 2 = 0 THEN 'USER' ELSE 'ASSISTANT' END,
                    'ขอไข่ไก่ร้านป้าแดงสองแผงด้วยค่ะ ส่งที่บ้านได้ไหมคะ',
                    now() - (g::float8 / 4000) * interval '1 day'
             FROM generate_series(1, 120000) g"
        ),
    )
    .await?;

    step(&db, "vacuum", "VACUUM ANALYZE").await?;
    println!("seeded");
    Ok(())
}
