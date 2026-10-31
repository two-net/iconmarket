//! The run: phases of open-loop load, then what it did to the latencies and the data.
//!
//! Journeys start on a Poisson schedule that nothing in the app can slow down, and each event's
//! latency counts from when it was due, so a backlog shows up in the numbers instead of quietly
//! lowering the load. Then the database is checked against what the villagers were told.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::journeys::{self, Market, Person};
use crate::line::{self, Line};
use crate::seed::{LISTING_SHOPS, line_id};
use crate::{Options, Random, jpeg};

/// What's measured.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
    /// From when the event was due until the webhook answered (NFR-PERF-01)
    Ack,
    /// From then until the reply reached LINE (NFR-PERF-02)
    Reply,
    /// Likewise, for a photo (NFR-PERF-04)
    ImageReply,
    /// How late the generator sent it; high means the run itself was short of CPU
    Lateness,
    /// A product photo fetched as LINE's CDN does
    Photo,
}

impl Kind {
    const ALL: [Self; 5] = [
        Self::Ack,
        Self::Reply,
        Self::ImageReply,
        Self::Lateness,
        Self::Photo,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::Reply => "reply",
            Self::ImageReply => "image reply",
            Self::Lateness => "send lateness",
            Self::Photo => "photo fetch",
        }
    }
}

#[derive(Clone, Debug)]
struct Phase {
    name: String,
    /// Events per second aimed for
    rate: f64,
    seconds: u64,
}

/// `name:rate:seconds`, comma-separated.
fn phases(text: &str) -> anyhow::Result<Vec<Phase>> {
    text.split(',')
        .map(|phase| {
            let parts: Vec<_> = phase.split(':').collect();
            let [name, rate, seconds] = parts[..] else {
                anyhow::bail!("--phases: {phase} isn't name:rate:seconds");
            };
            Ok(Phase {
                name: name.to_owned(),
                rate: rate.parse().context("--phases: rate")?,
                seconds: seconds.parse().context("--phases: seconds")?,
            })
        })
        .collect()
}

/// Every latency of a run, as (when it was due, kind, how long).
#[derive(Default)]
pub struct Metrics {
    samples: Mutex<Vec<(Duration, Kind, Duration)>>,
    failures: Mutex<HashMap<String, u64>>,
    finished: Mutex<HashMap<&'static str, u64>>,
}

impl Metrics {
    pub fn record(&self, kind: Kind, at: Duration, took: Duration) {
        self.samples.lock().unwrap().push((at, kind, took));
    }

    fn finish(&self, journey: &'static str, result: Result<(), String>) {
        match result {
            Ok(()) => *self.finished.lock().unwrap().entry(journey).or_default() += 1,
            Err(why) => {
                let key = format!("{journey}: {why}");
                *self.failures.lock().unwrap().entry(key).or_default() += 1;
            }
        }
    }
}

/// The value at `percent` of `sorted`, in ms.
fn percentile(sorted: &[Duration], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (sorted.len() - 1) as f64 * percent / 100.0;
    sorted[rank.round() as usize].as_secs_f64() * 1_000.0
}

/// Starts per second of each journey when the load is 20 events per second, the peak mix.
const MIX: [(&str, f64); 6] = [
    ("browse", 1.4),
    ("search", 0.8),
    ("cash", 0.7),
    ("promptpay", 0.2),
    ("listing", 0.08),
    ("join", 0.15),
];

/// Starts `journey` as the mix has it. A person comes from their pool and, if the journey went
/// to plan, goes back to it; one left in the middle of a flow would only confuse the next.
fn begin(market: &Arc<Market>, journey: &'static str, tasks: &mut JoinSet<()>) {
    let market = Arc::clone(market);
    let now = Instant::now();
    let pool = match journey {
        "listing" => Some(&market.listers),
        "join" => None,
        _ => Some(&market.buyers),
    };
    let id = match pool {
        Some(pool) => pool.lock().unwrap().pop_front(),
        None => Some(market.new_user()),
    };
    let Some(id) = id else {
        market
            .metrics
            .finish(journey, Err("nobody free".to_owned()));
        return;
    };
    let person = Person::new(&market, id.clone(), now);
    let run = async move {
        let result = match journey {
            "browse" => journeys::browses(person).await,
            "search" => journeys::searches(person).await,
            "cash" => journeys::pays_cash(person).await,
            "promptpay" => journeys::pays_by_promptpay(person).await,
            "listing" => journeys::lists(person).await,
            _ => journeys::joins(person).await,
        };
        if result.is_ok() {
            match journey {
                "listing" => market.listers.lock().unwrap().push_back(id),
                "join" => {}
                _ => market.buyers.lock().unwrap().push_back(id),
            }
        }
        market.metrics.finish(journey, result);
    };
    tasks.spawn(run);
}

/// The journeys of `phases`, each starting at random at the mix's rate, times `calibration`, as
/// journeys send more or fewer events than the mix counts on.
async fn load(market: &Arc<Market>, phases: &[Phase], calibration: f64) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    let total: f64 = MIX.iter().map(|(_, starts)| starts).sum();
    let mut random = Random::new(11);
    let mut next = market.started;
    for phase in phases {
        let end = next + Duration::from_secs(phase.seconds);
        let starts = phase.rate / 20.0 * total * calibration;
        while next < end {
            tokio::time::sleep_until(next.into()).await;
            let mut choice = random.fraction() * total;
            let journey = MIX
                .iter()
                .find(|(_, starts)| {
                    choice -= starts;
                    choice < 0.0
                })
                .map_or("browse", |(journey, _)| journey);
            begin(market, journey, &mut tasks);
            while tasks.try_join_next().is_some() {}
            let gap = -(1.0 - random.fraction()).ln() / starts;
            next += Duration::from_secs_f64(gap);
        }
        next = end;
    }
    tasks
}

/// The sellers' side: whatever a push asks them to do, they do after thinking it over. Each is
/// one person, so a busy seller's orders wait their turn instead of being tapped at once.
async fn sell(market: Arc<Market>, mut pushes: mpsc::UnboundedReceiver<(String, Value)>) {
    let mut sellers: HashMap<String, Arc<tokio::sync::Mutex<()>>> = HashMap::new();
    while let Some((to, push)) = pushes.recv().await {
        let market = Arc::clone(&market);
        let seller = Arc::clone(sellers.entry(to.clone()).or_default());
        tokio::spawn(async move {
            let _their_turn = seller.lock().await;
            let think = Duration::from_millis(market.random.lock().unwrap().between(3_000, 8_000));
            let person = Person::new(&market, to, Instant::now() + think);
            let result = journeys::sells(person, push).await;
            market.metrics.finish("seller", result);
        });
    }
}

/// Every 5 s, the deepest the inbox has been, and a job told to run once when due.
async fn watch(
    db: PgPool,
    market: Arc<Market>,
    retention_at: Option<u64>,
    stop: Arc<AtomicBool>,
    deepest: Arc<AtomicU64>,
) {
    let mut retention_at = retention_at;
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let depth: Result<i64, _> = sqlx::query_scalar(
            "SELECT count(*) FROM webhook_events WHERE status IN ('PENDING', 'PROCESSING')",
        )
        .fetch_one(&db)
        .await;
        if let Ok(depth) = depth {
            deepest.fetch_max(u64::try_from(depth).unwrap_or(0), Ordering::Relaxed);
        }
        if retention_at.is_some_and(|at| market.started.elapsed().as_secs() >= at) {
            retention_at = None;
            let due = "UPDATE jobs SET due_at = now() WHERE name = 'apply-retention'";
            let _ = sqlx::query(due).execute(&db).await;
            println!("retention job due now");
        }
    }
}

fn http() -> anyhow::Result<reqwest::Client> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()?;
    Ok(client)
}

async fn until_healthy(http: &reqwest::Client, app: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let answer = http.get(format!("{app}/health")).send().await;
        if answer.is_ok_and(|answer| answer.status().is_success()) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "the app never became healthy");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// What the run left in the database, checked against what it was told.
async fn audit(db: &PgPool, market: &Market, report: &mut String) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let waiting = loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM webhook_events WHERE status IN ('PENDING', 'PROCESSING')",
        )
        .fetch_one(db)
        .await?;
        if waiting == 0 || Instant::now() > deadline {
            break waiting;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let these = format!("load-{}-%", market.run);
    let count = |sql: &'static str| {
        let these = these.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(these)
                .fetch_one(db)
                .await
        }
    };
    let failed =
        count("SELECT count(*) FROM webhook_events WHERE event_id LIKE $1 AND status = 'FAILED'")
            .await?;
    let retried =
        count("SELECT count(*) FROM webhook_events WHERE event_id LIKE $1 AND attempts > 1")
            .await?;
    let stored = count("SELECT count(*) FROM webhook_events WHERE event_id LIKE $1").await?;
    let orders = count(
        "SELECT count(*) FROM orders WHERE created_at >= (
             SELECT min(received_at) FROM webhook_events WHERE event_id LIKE $1)",
    )
    .await?;
    let oversold: i64 = sqlx::query_scalar("SELECT count(*) FROM products WHERE stock < 0")
        .fetch_one(db)
        .await?;
    let placed = market.orders_placed.load(Ordering::Relaxed);
    let oddities = &market.line.oddities;
    let stray = oddities.replies_without_a_waiter.load(Ordering::Relaxed);
    writeln!(
        report,
        "\n| Check | Result |\n| --- | --- |\n\
         | Inbox left waiting | {waiting} |\n\
         | Events stored | {stored} |\n\
         | Events failed for good | {failed} |\n\
         | Events retried | {retried} |\n\
         | Orders told to buyers / in the database | {placed} / {orders} |\n\
         | Products oversold | {oversold} |\n\
         | Replies nobody was waiting for | {stray} |\n\
         | Pushes | {} |",
        oddities.pushes.load(Ordering::Relaxed)
    )?;
    Ok(json!({
        "waiting": waiting,
        "stored": stored,
        "failed": failed,
        "retried": retried,
        "ordersPlaced": placed,
        "ordersInDatabase": orders,
        "oversold": oversold,
        "strayReplies": stray,
    }))
}

/// The slowest searches and the statements taking the most time, from `pg_stat_statements`.
async fn statements(admin: &PgPool, report: &mut String) -> anyhow::Result<Value> {
    let search: Option<(i64, f64, f64)> = sqlx::query_as(
        "SELECT sum(calls)::bigint, max(max_exec_time), sum(total_exec_time) / sum(calls)
         FROM pg_stat_statements WHERE query ILIKE '%products%' AND query LIKE '%ILIKE%'
         HAVING sum(calls) > 0",
    )
    .fetch_optional(admin)
    .await?;
    let top: Vec<(String, i64, f64, f64)> = sqlx::query_as(
        "SELECT left(regexp_replace(query, '\\s+', ' ', 'g'), 70), calls::bigint,
                total_exec_time, max_exec_time
         FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 8",
    )
    .fetch_all(admin)
    .await?;
    writeln!(
        report,
        "\n| Statement | Calls | Total ms | Max ms |\n| --- | --- | --- | --- |"
    )?;
    for (query, calls, total, max) in &top {
        let query = query.replace('|', "\\|");
        writeln!(report, "| `{query}` | {calls} | {total:.0} | {max:.1} |")?;
    }
    let (calls, max, mean) = search.unwrap_or_default();
    writeln!(
        report,
        "\nSearches in the database: {calls}, slowest {max:.1} ms, mean {mean:.1} ms."
    )?;
    Ok(json!({ "calls": calls, "maxMs": max, "meanMs": mean }))
}

pub async fn run(options: &Options) -> anyhow::Result<()> {
    let db = options.database("db", 2).await?;
    let app = options.text("app", "http://127.0.0.1:18080");
    let phases = phases(&options.text("phases", "cold:5:120,warm:5:480,peak:20:900"))?;
    let shops: usize = options.number("shops", 300)?;
    let users: usize = options.number("users", 3_000)?;
    let retention_at: u64 = options.number("retention-at", 0)?;
    let burst: usize = options.number("burst", 0)?;
    let calibration: f64 = options.number("calibration", 1.0)?;
    let latency = options.text("line-ms", "100-200");
    let (low, high) = latency.split_once('-').context("--line-ms is low-high")?;
    let latency = (low.parse()?, high.parse()?);

    let photo = jpeg(4_000, 3_000, 24, 90)?;
    println!("a user's photo is {} KB", photo.len() / 1024);
    let (seller_pushes, pushes) = mpsc::unbounded_channel();
    let fake = Arc::new(Line::new(photo, latency, seller_pushes));
    line::serve(
        Arc::clone(&fake),
        &options.text("line-api", "0.0.0.0:9001"),
        &options.text("line-data", "0.0.0.0:9002"),
    )
    .await?;

    let http = http()?;
    until_healthy(&http, &app).await?;
    let admin = match options.text("admin-db", "").as_str() {
        "" => None,
        _ => Some(options.database("admin-db", 1).await?),
    };
    if let Some(admin) = &admin {
        sqlx::query("SELECT pg_stat_statements_reset()")
            .execute(admin)
            .await?;
    }
    let categories: Vec<String> = sqlx::query_scalar("SELECT name FROM categories ORDER BY id")
        .fetch_all(&db)
        .await?;
    anyhow::ensure!(!categories.is_empty(), "no categories: is it migrated?");

    let selling = LISTING_SHOPS as usize;
    let mut random = Random::new(13);
    let mut buyers: Vec<_> = (shops + 1..=users).map(line_id).collect();
    for i in (1..buyers.len()).rev() {
        buyers.swap(i, random.between(0, i as u64 + 1) as usize);
    }
    let market = Arc::new(Market {
        http,
        app,
        secret: options.text("secret", "loadtest-channel-secret"),
        line: fake,
        metrics: Metrics::default(),
        categories,
        buyers: Mutex::new(VecDeque::from(buyers)),
        listers: Mutex::new((1..=selling).map(line_id).collect()),
        started: Instant::now(),
        random: Mutex::new(Random::new(17)),
        run: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        numbers: AtomicU64::new(1),
        fetched: Mutex::new(HashSet::new()),
        orders_placed: AtomicU64::new(0),
    });
    tokio::spawn(sell(Arc::clone(&market), pushes));
    let (stop, deepest) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
    );
    let retention_at = (retention_at > 0).then_some(retention_at);
    let watcher = tokio::spawn(watch(
        db.clone(),
        Arc::clone(&market),
        retention_at,
        Arc::clone(&stop),
        Arc::clone(&deepest),
    ));

    println!("load starts");
    let mut journeys = load(&market, &phases, calibration).await;
    let ran_for = market.started.elapsed();
    println!("load over; finishing journeys under way");
    let finishing = async { while journeys.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(Duration::from_secs(180), finishing).await;
    if burst > 0 {
        burst_of_photos(&market, burst).await;
    }
    // Sellers still at work
    tokio::time::sleep(Duration::from_secs(30)).await;
    stop.store(true, Ordering::Relaxed);
    let _ = watcher.await;

    let mut report = String::from(
        "| Phase | Measure | Count | p50 ms | p95 ms | p99 ms | Max ms |\n| --- | --- | --- | --- | --- | --- | --- |\n",
    );
    let results = phase_report(&market, &phases, ran_for, &mut report)?;
    let audit = audit(&db, &market, &mut report).await?;
    let searches = match &admin {
        Some(admin) => statements(admin, &mut report).await?,
        None => Value::Null,
    };
    {
        let failures = market.metrics.failures.lock().unwrap();
        let finished = market.metrics.finished.lock().unwrap();
        writeln!(report, "\nJourneys finished: {finished:?}")?;
        writeln!(report, "\nJourneys failed: {failures:?}")?;
    }
    writeln!(
        report,
        "\nDeepest inbox seen: {}",
        deepest.load(Ordering::Relaxed)
    )?;
    println!("{report}");

    let summary = json!({
        "phases": results,
        "audit": audit,
        "searches": searches,
        "deepestInbox": deepest.load(Ordering::Relaxed),
    });
    let out = options.text("out", "");
    if !out.is_empty() {
        std::fs::write(&out, format!("{report}\n<!-- {summary} -->\n"))?;
    }
    Ok(())
}

/// `burst` sellers' photos within a moment, each from a seller who is on the photo step.
async fn burst_of_photos(market: &Arc<Market>, burst: usize) {
    let mut tasks = JoinSet::new();
    let at = Instant::now() + Duration::from_secs(10);
    for _ in 0..burst {
        let Some(id) = market.listers.lock().unwrap().pop_front() else {
            break;
        };
        let market = Arc::clone(market);
        tasks.spawn(async move {
            let person = Person::new(&market, id, Instant::now());
            let result = journeys::starts_listing_with_a_photo(person, at).await;
            market.metrics.finish("burst", result);
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// One row per phase and measure, and the rate each phase reached.
fn phase_report(
    market: &Market,
    phases: &[Phase],
    ran_for: Duration,
    report: &mut String,
) -> anyhow::Result<Value> {
    let samples = market.metrics.samples.lock().unwrap();
    let mut results = Vec::new();
    let mut from = Duration::ZERO;
    let mut windows: Vec<(String, Duration, Duration, f64)> = Vec::new();
    for phase in phases {
        let until = from + Duration::from_secs(phase.seconds);
        windows.push((phase.name.clone(), from, until, phase.rate));
        from = until;
    }
    windows.push(("after".to_owned(), ran_for, Duration::MAX, 0.0));
    for (name, from, until, rate) in windows {
        let within = |at: Duration| from <= at && at < until;
        let mut events = 0_u64;
        for kind in Kind::ALL {
            let mut took: Vec<_> = samples
                .iter()
                .filter(|(at, measured, _)| *measured == kind && within(*at))
                .map(|(_, _, took)| *took)
                .collect();
            if took.is_empty() {
                continue;
            }
            took.sort_unstable();
            let (p50, p95, p99) = (
                percentile(&took, 50.0),
                percentile(&took, 95.0),
                percentile(&took, 99.0),
            );
            let max = took[took.len() - 1].as_secs_f64() * 1_000.0;
            writeln!(
                report,
                "| {name} | {} | {} | {p50:.0} | {p95:.0} | {p99:.0} | {max:.0} |",
                kind.name(),
                took.len()
            )?;
            results.push(json!({
                "phase": name, "kind": kind.name(), "count": took.len(),
                "p50": p50, "p95": p95, "p99": p99, "max": max,
            }));
            if kind == Kind::Lateness {
                events = took.len() as u64;
            }
        }
        let seconds = (until.min(ran_for) - from.min(ran_for)).as_secs_f64();
        if seconds > 0.0 {
            let achieved = events as f64 / seconds;
            // A phase short of its rate proves nothing about that rate
            let enough = if achieved >= rate * 0.98 {
                "reached"
            } else {
                "SHORT"
            };
            writeln!(
                report,
                "| {name} | events/s | {events} | achieved {achieved:.1} | wanted {rate:.1} | {enough} | |"
            )?;
            results.push(json!({ "phase": name, "achieved": achieved, "wanted": rate }));
        }
    }
    Ok(Value::Array(results))
}
