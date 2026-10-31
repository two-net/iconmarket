//! A load test of the real binary at the target scale (NFR-CAP-01, NFR-PERF-01 – NFR-PERF-05).
//! `cargo test` builds it, so it keeps compiling; it never ships. VERIFICATION.md has how to run
//! it and what it found.
//!
//! - `seed` fills a database the app has already migrated, straight in SQL.
//! - `run` is a fake LINE (`line.apiBaseUrl`, `line.dataBaseUrl`) and virtual villagers, who sign
//!   webhooks and tap what the replies offer, then reports latencies from when each event was
//!   due to be sent, so a slow app can't hold back its own load.
//! - `size` reports each table's size and projects it over the retention windows of SRS §5.4.

mod journeys;
mod line;
mod run;
mod seed;
mod size;

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, anyhow, bail};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

const USAGE: &str = "usage: loadtest <seed|run|size> [--name value]...

common:  --db postgres://user:password@host:port/name
seed:    --users 3000 --shops 300 --products 10000 --order-days 365
         --event-days 15 --events-per-day 40000 --real-image-percent 10 --photo-kb 150
run:     --app http://host:port --secret S --line-api ADDR --line-data ADDR
         --phases name:rate:seconds,... --calibration 1.0 --retention-at SECONDS
         --burst 8 --line-ms 100-200 --admin-db URL --out FILE
size:    --events-per-day 40000 --ai-calls-per-day 2000 --photo-kb 150 --out FILE";

/// `--name value` pairs.
pub struct Options(HashMap<String, String>);

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut options = HashMap::new();
        while let Some(name) = args.next() {
            let name = name.strip_prefix("--").ok_or_else(|| anyhow!(USAGE))?;
            let value = args.next().ok_or_else(|| anyhow!(USAGE))?;
            options.insert(name.to_owned(), value);
        }
        Ok(Self(options))
    }

    pub fn text(&self, name: &str, default: &str) -> String {
        self.0
            .get(name)
            .map_or_else(|| default.to_owned(), String::clone)
    }

    pub fn number<T: FromStr>(&self, name: &str, default: T) -> anyhow::Result<T> {
        self.0.get(name).map_or(Ok(default), |value| {
            value
                .parse()
                .map_err(|_| anyhow!("--{name}: {value} is not a number"))
        })
    }

    /// The database, as a pool of `connections`.
    pub async fn database(&self, name: &str, connections: u32) -> anyhow::Result<PgPool> {
        let url = self
            .0
            .get(name)
            .with_context(|| format!("--{name} is required"))?;
        let options = PgConnectOptions::from_str(url)
            .with_context(|| format!("--{name} is not a database URL"))?;
        let pool = PgPoolOptions::new()
            .max_connections(connections)
            .connect_with(options)
            .await
            .with_context(|| format!("cannot connect to --{name}"))?;
        Ok(pool)
    }
}

/// splitmix64: the only randomness needed, and repeatable from its seed.
#[derive(Clone, Debug)]
pub struct Random(u64);

impl Random {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn number(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// In [0, 1).
    pub fn fraction(&mut self) -> f64 {
        (self.number() >> 11) as f64 / (1_u64 << 53) as f64
    }

    /// In `low..high`.
    pub fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.number() % (high - low).max(1)
    }
}

/// A JPEG of `width` × `height` as noisy as a photo, so that it costs what one does to decode
/// and doesn't compress further.
pub fn jpeg(width: u32, height: u32, grain: u8, quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut random = Random::new(u64::from(width) << 32 | u64::from(height));
    let pixels = image::RgbImage::from_fn(width, height, |x, y| {
        let wave = |at: u32, size: u32| (f64::from(at) / f64::from(size) * 255.0) as u8;
        let noise = random.number().to_le_bytes();
        let shade = |base: u8, byte: u8| base.saturating_add(byte % grain.max(1));
        image::Rgb([
            shade(wave(x, width), noise[0]),
            shade(wave(y, height), noise[1]),
            shade(wave(x + y, width + height), noise[2]),
        ])
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(&pixels)
        .context("cannot encode a JPEG")?;
    Ok(bytes)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let options = Options::parse(args)?;
    match command.as_str() {
        "seed" => seed::seed(&options).await,
        "run" => run::run(&options).await,
        "size" => size::size(&options).await,
        _ => bail!(USAGE),
    }
}
