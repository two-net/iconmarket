//! NFR-CAP-03: how big the database gets once retention bounds it. Each table's bytes per row
//! are measured from the seeded database, its rows projected from SRS §5.4 at the target load,
//! and photos counted at `--photo-kb`, as only a sample of them hold real bytes here.

use std::fmt::Write as _;

use crate::Options;

/// How a table's rows grow.
enum Rows {
    /// Bounded by the target, whatever the age
    Fixed(f64),
    /// That many a day, kept for that many days
    Kept(f64, f64),
    /// That many a day, kept for good
    Forever(f64),
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub async fn size(options: &Options) -> anyhow::Result<()> {
    let events_per_day: f64 = options.number("events-per-day", 40_000.0)?;
    let ai_calls_per_day: f64 = options.number("ai-calls-per-day", 2_000.0)?;
    let photo_kb: f64 = options.number("photo-kb", 150.0)?;
    let db = options.database("db", 1).await?;

    let measured: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT c.relname::text, pg_total_relation_size(c.oid), c.reltuples::bigint
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind = 'r' AND n.nspname = 'public' ORDER BY 2 DESC",
    )
    .fetch_all(&db)
    .await?;
    let total: i64 = measured.iter().map(|(_, bytes, _)| bytes).sum();
    let mut report = format!(
        "Seeded database: {:.2} GB\n\n| Table | Rows | MB | Bytes a row |\n| --- | --- | --- | --- |\n",
        total as f64 / GB
    );
    let per_row = |table: &str| {
        measured
            .iter()
            .find(|(name, ..)| name == table)
            .map_or(0.0, |(_, bytes, rows)| {
                *bytes as f64 / (*rows).max(1) as f64
            })
    };
    for (table, bytes, rows) in &measured {
        let mb = *bytes as f64 / (1024.0 * 1024.0);
        writeln!(
            report,
            "| {table} | {rows} | {mb:.1} | {:.0} |",
            *bytes as f64 / (*rows).max(1) as f64
        )?;
    }

    // Measured in a run, or above it: 1 item, 3.92 moves, 25 events and 5.1 pushes an order
    let image_row: f64 = sqlx::query_scalar(
        "SELECT avg(pg_column_size(i) - size_bytes)::float8 + 200 FROM images i WHERE size_bytes < 2000",
    )
    .fetch_one(&db)
    .await?;
    let photo = photo_kb * 1024.0 + image_row;
    let promptpay_per_day = 400.0;
    let tables = [
        ("users", Rows::Fixed(3_000.0), per_row("users")),
        ("shops", Rows::Fixed(300.0), per_row("shops")),
        ("products", Rows::Fixed(10_000.0), per_row("products")),
        ("orders", Rows::Forever(1_000.0), per_row("orders")),
        (
            "order_items",
            Rows::Forever(1_500.0),
            per_row("order_items"),
        ),
        (
            "order_events",
            Rows::Forever(4_000.0),
            per_row("order_events"),
        ),
        (
            "payment_slips",
            Rows::Forever(promptpay_per_day),
            per_row("payment_slips"),
        ),
        (
            "webhook_events",
            Rows::Kept(events_per_day, 14.0),
            per_row("webhook_events"),
        ),
        ("push_logs", Rows::Kept(5_100.0, 90.0), per_row("push_logs")),
        ("audit_logs", Rows::Kept(15.0, 365.0), per_row("audit_logs")),
        (
            "ai_messages",
            Rows::Kept(ai_calls_per_day * 2.0, 30.0),
            per_row("ai_messages"),
        ),
        (
            "ai_call_logs",
            Rows::Kept(ai_calls_per_day, 90.0),
            per_row("ai_call_logs"),
        ),
        ("product photos, 2 each", Rows::Fixed(20_000.0), photo),
        ("shop photos", Rows::Fixed(300.0), photo),
        ("slip photos", Rows::Kept(promptpay_per_day, 90.0), photo),
    ];
    let bytes = |rows: &Rows, row: f64, years: f64| match rows {
        Rows::Fixed(count) => count * row,
        Rows::Kept(per_day, days) => per_day * days * row,
        Rows::Forever(per_day) => per_day * 365.0 * years * row,
    };

    writeln!(
        report,
        "\nProjection at {events_per_day} events and 1,000 orders a day, {photo_kb} KB a photo, GB\n\n\
         | Table | 1 year | 3 years | 5 years |\n| --- | --- | --- | --- |"
    )?;
    let mut totals = [0.0; 3];
    for (table, rows, row) in &tables {
        let [a, b, c] = [1.0, 3.0, 5.0].map(|years| bytes(rows, *row, years) / GB);
        writeln!(report, "| {table} | {a:.2} | {b:.2} | {c:.2} |")?;
        totals[0] += a;
        totals[1] += b;
        totals[2] += c;
    }
    writeln!(
        report,
        "| **Total** | **{:.1}** | **{:.1}** | **{:.1}** |",
        totals[0], totals[1], totals[2]
    )?;
    // Space autovacuum frees is reused, not returned, so the log tables run this far above
    // their live rows; a third is a common working figure for tables with steady deletes
    writeln!(
        report,
        "| Total with a third of free space | {:.1} | {:.1} | {:.1} |",
        totals[0] * 1.33,
        totals[1] * 1.33,
        totals[2] * 1.33
    )?;

    writeln!(
        report,
        "\nFive years by photo size, with free space, GB\n\n| KB a photo | Total |\n| --- | --- |"
    )?;
    for kb in [100.0, 150.0, 250.0, 400.0] {
        let scale = (kb * 1024.0 + image_row) / photo;
        let total: f64 = tables
            .iter()
            .map(|(table, rows, row)| {
                let row = if table.contains("photo") {
                    row * scale
                } else {
                    *row
                };
                bytes(rows, row, 5.0) / GB
            })
            .sum();
        writeln!(report, "| {kb} | {:.1} |", total * 1.33)?;
    }
    println!("{report}");
    let out = options.text("out", "");
    if !out.is_empty() {
        std::fs::write(out, &report)?;
    }
    Ok(())
}
