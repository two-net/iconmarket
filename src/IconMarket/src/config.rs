//! The configuration file: the only source of configuration, credentials included
//! (CON-08, CON-09, SRS §6). Key names follow §6.2 (`app.listenPort`).
//!
//! The file is TOML (CFG-04). It is read once at startup, so changes take effect on
//! restart (CFG-08). Loading reports every problem together (CFG-05), lists unknown keys
//! (CFG-07) and never prints a value, so diagnostics can't leak a secret (CFG-06).

#![allow(
    dead_code,
    reason = "§6.2 is modelled in full; each section is read by the module that owns it as later issues build it"
)]

mod reader;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use chrono::NaiveTime;
use chrono_tz::Tz;
use sqlx::postgres::PgSslMode;
use toml::{Table, Value};
use tracing::level_filters::LevelFilter;
use url::Url;

use reader::{
    Reader, Report, at_least, boolean, https_url, line_id, list, non_empty, number, one_of, port,
    secret, string, time_of_day, time_zone, web_url,
};

/// Read from the working directory unless the first command-line argument names another file.
pub const DEFAULT_PATH: &str = "iconmarket.toml";

#[derive(Debug)]
pub struct Config {
    pub app: AppConfig,
    pub village: VillageConfig,
    pub line: LineConfig,
    pub ai: AiConfig,
    pub database: DatabaseConfig,
    pub shop: ShopConfig,
    pub order: OrderConfig,
    pub payment: PaymentConfig,
    pub notification: NotificationConfig,
    pub image: ImageConfig,
    pub session: SessionConfig,
    pub limits: LimitsConfig,
    pub catalog: CatalogConfig,
    pub privacy: PrivacyConfig,
    pub admin: AdminConfig,
    pub logging: LoggingConfig,
    pub retention: RetentionConfig,
}

#[derive(Debug)]
pub struct AppConfig {
    pub name: String,
    pub base_url: Url,
    pub listen_port: u16,
    pub time_zone: Tz,
    pub polite_particle: PoliteParticle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PoliteParticle {
    /// ค่ะ
    Kha,
    /// ครับ
    Khrap,
}

impl PoliteParticle {
    const CHOICES: &'static [(&'static str, Self)] = &[("ค่ะ", Self::Kha), ("ครับ", Self::Khrap)];
}

#[derive(Debug)]
pub struct VillageConfig {
    pub name: String,
    pub zones: Vec<String>,
    /// `None` means registration needs no code
    pub join_code: Option<String>,
    pub admin_contact: Option<String>,
}

#[derive(Debug)]
pub struct LineConfig {
    pub channel_secret: Secret,
    pub channel_access_token: Secret,
    pub rich_menu: RichMenuConfig,
    pub village_group_id: Option<String>,
}

#[derive(Debug)]
pub struct RichMenuConfig {
    pub auto_provision: bool,
}

#[derive(Debug)]
pub struct AiConfig {
    /// `Some` exactly when `ai.enabled`
    pub primary: Option<AiEndpoint>,
    pub base_url: Option<Url>,
    pub timeout_seconds: u32,
    pub max_output_tokens: u32,
    pub temperature: f64,
    pub history_turns: u32,
    pub features: AiFeatures,
    pub limits: AiLimits,
    pub log_content: bool,
    /// `Some` when `ai.fallback.provider` is set
    pub fallback: Option<AiEndpoint>,
}

#[derive(Debug)]
pub struct AiEndpoint {
    pub provider: AiProvider,
    pub model: String,
    /// `None` only for the mock provider
    pub api_key: Option<Secret>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiProvider {
    Claude,
    Gemini,
    Gpt,
    Grok,
    /// Deterministic, no network and no key (FR-AIP-05)
    Mock,
}

impl AiProvider {
    const CHOICES: &'static [(&'static str, Self)] = &[
        ("claude", Self::Claude),
        ("gemini", Self::Gemini),
        ("gpt", Self::Gpt),
        ("grok", Self::Grok),
        ("mock", Self::Mock),
    ];

    /// As written in the config file
    pub fn name(self) -> &'static str {
        Self::CHOICES
            .iter()
            .find(|(_, provider)| *provider == self)
            .map(|(name, _)| *name)
            .expect("every provider is one of the choices")
    }
}

#[derive(Debug)]
pub struct AiFeatures {
    pub assistant: bool,
    pub listing: bool,
    pub moderation: bool,
    pub slip_reading: bool,
    pub voice: bool,
    pub visual_search: bool,
}

#[derive(Debug)]
pub struct AiLimits {
    pub per_user_daily_calls: u32,
    pub global_daily_calls: u32,
}

#[derive(Debug)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: Secret,
    pub ssl_mode: PgSslMode,
    pub max_connections: u32,
}

const SSL_MODES: &[(&str, PgSslMode)] = &[
    ("disable", PgSslMode::Disable),
    ("allow", PgSslMode::Allow),
    ("prefer", PgSslMode::Prefer),
    ("require", PgSslMode::Require),
    ("verify-ca", PgSslMode::VerifyCa),
    ("verify-full", PgSslMode::VerifyFull),
];

#[derive(Debug)]
pub struct ShopConfig {
    pub require_approval: bool,
}

#[derive(Debug)]
pub struct OrderConfig {
    pub pending_timeout_minutes: u32,
}

#[derive(Debug)]
pub struct PaymentConfig {
    pub methods: Vec<PaymentMethod>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaymentMethod {
    Cash,
    PromptPay,
}

impl PaymentMethod {
    const CHOICES: &'static [(&'static str, Self)] =
        &[("cash", Self::Cash), ("promptpay", Self::PromptPay)];
}

#[derive(Debug)]
pub struct NotificationConfig {
    pub new_order_to_seller: bool,
    pub order_status_to_buyer: bool,
    pub admin_alerts: bool,
    pub daily_summary: bool,
    pub daily_summary_time: NaiveTime,
    pub group_digest: bool,
    pub group_digest_time: NaiveTime,
}

/// Cheap to clone, for image processing on the blocking pool.
#[derive(Debug, Clone)]
pub struct ImageConfig {
    pub max_upload_mb: u32,
    pub max_dimension_px: u32,
    pub max_per_product: u32,
    pub slip_link_hours: u32,
}

#[derive(Debug)]
pub struct SessionConfig {
    pub timeout_minutes: u32,
    pub pending_action_minutes: u32,
}

#[derive(Debug)]
pub struct LimitsConfig {
    pub messages_per_user_per_minute: u32,
}

#[derive(Debug)]
pub struct CatalogConfig {
    pub default_categories: Vec<String>,
    pub prohibited_keywords: Vec<String>,
}

/// §6.2's suggested categories, seeded on first start only (MIG-05).
const DEFAULT_CATEGORIES: [&str; 10] = [
    "อาหาร",
    "ขนม/ของหวาน",
    "เครื่องดื่ม",
    "ผัก/ผลไม้",
    "เนื้อสัตว์/ไข่",
    "ของใช้ในบ้าน",
    "ต้นไม้/การเกษตร",
    "งานฝีมือ",
    "บริการ",
    "อื่นๆ",
];

#[derive(Debug)]
pub struct PrivacyConfig {
    pub notice_version: String,
    pub notice_text: String,
    pub notice_url: Option<Url>,
}

#[derive(Debug)]
pub struct AdminConfig {
    /// At least one
    pub line_user_ids: Vec<String>,
}

#[derive(Debug)]
pub struct LoggingConfig {
    pub level: LogLevel,
    /// Empty means standard output.
    pub file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    const CHOICES: &'static [(&'static str, Self)] = &[
        ("debug", Self::Debug),
        ("info", Self::Info),
        ("warn", Self::Warn),
        ("error", Self::Error),
    ];
}

impl From<LogLevel> for LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Error => LevelFilter::ERROR,
        }
    }
}

#[derive(Debug)]
pub struct RetentionConfig {
    pub webhook_event_days: u32,
    pub ai_message_days: u32,
    pub ai_call_log_days: u32,
    pub push_log_days: u32,
    pub slip_days: u32,
    pub deleted_image_days: u32,
    pub audit_log_days: u32,
}

/// A value that must never be logged, shown or sent anywhere but its destination (CFG-06).
/// `Debug` prints a mask, and there is no `Display` or `Serialize` to leak it through.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub fn new(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"***\"")
    }
}

pub fn path_from_args(args: impl IntoIterator<Item = OsString>) -> PathBuf {
    args.into_iter()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_PATH), PathBuf::from)
}

/// A valid configuration and the unknown keys its file contained, for a startup warning.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub unknown_keys: Vec<String>,
}

/// Every problem found in a file, with its unknown keys since a typo often explains a
/// missing key. Displays as one line per item, each starting with a line break.
#[derive(Debug)]
struct Invalid {
    problems: Vec<String>,
    unknown_keys: Vec<String>,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for problem in &self.problems {
            write!(f, "\n  - {problem}")?;
        }
        for key in &self.unknown_keys {
            write!(f, "\nwarning: unknown key `{key}`, ignored")?;
        }
        Ok(())
    }
}

pub fn load(path: &Path) -> anyhow::Result<Loaded> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    load_str(&text).map_err(|invalid| anyhow!("invalid config file {}:{invalid}", path.display()))
}

/// The sample file's config, for tests to adjust.
#[cfg(test)]
pub fn sample() -> Config {
    load_str(include_str!("../iconmarket.sample.toml"))
        .unwrap_or_else(|invalid| panic!("the sample file is invalid:{invalid}"))
        .config
}

fn load_str(text: &str) -> Result<Loaded, Invalid> {
    let table = parse(text).map_err(|problem| Invalid {
        problems: vec![problem],
        unknown_keys: Vec::new(),
    })?;
    let (config, report) = read(&table);
    match config {
        Some(config) if report.problems.is_empty() => Ok(Loaded {
            config,
            unknown_keys: report.unknown_keys,
        }),
        _ => Err(Invalid {
            problems: report.problems,
            unknown_keys: report.unknown_keys,
        }),
    }
}

/// TOML syntax. A syntax error stops loading, since nothing after it can be trusted.
fn parse(text: &str) -> Result<Table, String> {
    text.parse::<Table>().map_err(|e| {
        // Message and position only: the full error quotes the offending line, which may
        // hold a secret
        let message = e.message().trim().replace('\n', "; ");
        match e.span() {
            Some(span) => {
                let (line, column) = line_column(text, span.start);
                format!("{message} (line {line}, column {column})")
            }
            None => message,
        }
    })
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

/// Reads every key of §6.2. `None` means a required key is missing or invalid; the report
/// has the problem.
fn read(table: &Table) -> (Option<Config>, Report) {
    let mut r = Reader::new(table);
    let config = read_sections(&mut r);
    (config, r.finish())
}

fn read_sections(r: &mut Reader) -> Option<Config> {
    // Read every section before giving up on any, so all problems are found in one run
    let app = app(r);
    let village = village(r);
    let line = line(r);
    let ai = ai(r);
    let database = database(r);
    let shop = ShopConfig {
        require_approval: r.optional("shop.requireApproval", true, boolean),
    };
    let order = OrderConfig {
        pending_timeout_minutes: r.optional("order.pendingTimeoutMinutes", 60, at_least(1)),
    };
    let payment = PaymentConfig {
        methods: r.optional(
            "payment.methods",
            vec![PaymentMethod::Cash, PaymentMethod::PromptPay],
            payment_methods,
        ),
    };
    let notification = notification(r);
    let image = ImageConfig {
        max_upload_mb: r.optional("image.maxUploadMB", 10, at_least(1)),
        max_dimension_px: r.optional("image.maxDimensionPx", 1024, at_least(1)),
        max_per_product: r.optional("image.maxPerProduct", 3, at_least(1)),
        slip_link_hours: r.optional("image.slipLinkHours", 24, at_least(1)),
    };
    let session = SessionConfig {
        timeout_minutes: r.optional("session.timeoutMinutes", 15, at_least(1)),
        pending_action_minutes: r.optional("session.pendingActionMinutes", 60, at_least(1)),
    };
    let limits = LimitsConfig {
        messages_per_user_per_minute: r.optional(
            "limits.messagesPerUserPerMinute",
            20,
            at_least(1),
        ),
    };
    let catalog = CatalogConfig {
        default_categories: r.optional(
            "catalog.defaultCategories",
            DEFAULT_CATEGORIES.map(String::from).to_vec(),
            list(string),
        ),
        prohibited_keywords: r.optional("catalog.prohibitedKeywords", Vec::new(), list(string)),
    };
    let privacy = privacy(r);
    let admin_ids = r.required("admin.lineUserIds", non_empty(list(line_id('U', "user"))));
    let logging = LoggingConfig {
        level: r.optional("logging.level", LogLevel::Info, one_of(LogLevel::CHOICES)),
        file: r.optional("logging.file", PathBuf::new(), |value| {
            string(value).map(PathBuf::from)
        }),
    };
    let retention = retention(r);

    Some(Config {
        app: app?,
        village: village?,
        line: line?,
        ai: ai?,
        database: database?,
        shop,
        order,
        payment,
        notification,
        image,
        session,
        limits,
        catalog,
        privacy: privacy?,
        admin: AdminConfig {
            line_user_ids: admin_ids?,
        },
        logging,
        retention,
    })
}

fn app(r: &mut Reader) -> Option<AppConfig> {
    let name = r.optional("app.name", "IconMarket".to_owned(), string);
    let base_url = r.required("app.baseUrl", https_url);
    let listen_port = r.optional("app.listenPort", 8080, port);
    let time_zone = r.optional("app.timeZone", chrono_tz::Asia::Bangkok, time_zone);
    let polite_particle = r.optional(
        "app.politeParticle",
        PoliteParticle::Kha,
        one_of(PoliteParticle::CHOICES),
    );
    Some(AppConfig {
        name,
        base_url: base_url?,
        listen_port,
        time_zone,
        polite_particle,
    })
}

fn village(r: &mut Reader) -> Option<VillageConfig> {
    let name = r.required("village.name", string);
    let zones = r.optional("village.zones", Vec::new(), list(string));
    let join_code = r.maybe("village.joinCode", string);
    let admin_contact = r.maybe("village.adminContact", string);
    Some(VillageConfig {
        name: name?,
        zones,
        join_code,
        admin_contact,
    })
}

fn line(r: &mut Reader) -> Option<LineConfig> {
    let channel_secret = r.required("line.channelSecret", secret);
    let channel_access_token = r.required("line.channelAccessToken", secret);
    let auto_provision = r.optional("line.richMenu.autoProvision", true, boolean);
    let village_group_id = r.maybe("line.villageGroupId", line_id('C', "group"));
    Some(LineConfig {
        channel_secret: channel_secret?,
        channel_access_token: channel_access_token?,
        rich_menu: RichMenuConfig { auto_provision },
        village_group_id,
    })
}

fn ai(r: &mut Reader) -> Option<AiConfig> {
    let enabled = r.optional("ai.enabled", true, boolean);
    let primary = ai_endpoint(r, "ai", enabled);
    let base_url = r.maybe("ai.baseUrl", web_url);
    let timeout_seconds = r.optional("ai.timeoutSeconds", 20, at_least(1));
    let max_output_tokens = r.optional("ai.maxOutputTokens", 1024, at_least(1));
    let temperature = r.optional("ai.temperature", 0.3, number(0.0..=2.0));
    let history_turns = r.optional("ai.historyTurns", 10, at_least(0));
    let features = AiFeatures {
        assistant: r.optional("ai.features.assistant", true, boolean),
        listing: r.optional("ai.features.listing", true, boolean),
        moderation: r.optional("ai.features.moderation", true, boolean),
        slip_reading: r.optional("ai.features.slipReading", false, boolean),
        voice: r.optional("ai.features.voice", false, boolean),
        visual_search: r.optional("ai.features.visualSearch", false, boolean),
    };
    let limits = AiLimits {
        per_user_daily_calls: r.optional("ai.limits.perUserDailyCalls", 50, at_least(1)),
        global_daily_calls: r.optional("ai.limits.globalDailyCalls", 2000, at_least(1)),
    };
    let log_content = r.optional("ai.logContent", false, boolean);
    let fallback_set = r.is_set("ai.fallback.provider");
    let fallback = ai_endpoint(r, "ai.fallback", fallback_set);

    // Incomplete endpoints have been reported
    if (enabled && primary.is_none()) || (fallback_set && fallback.is_none()) {
        return None;
    }
    Some(AiConfig {
        primary,
        base_url,
        timeout_seconds,
        max_output_tokens,
        temperature,
        history_turns,
        features,
        limits,
        log_content,
        fallback,
    })
}

/// `provider`, `model` and `apiKey` under `section`, required when `needed`; the mock
/// provider needs no key (FR-AIP-05). `None` when not needed, or when incomplete.
fn ai_endpoint(r: &mut Reader, section: &str, needed: bool) -> Option<AiEndpoint> {
    let provider = r.required_if(
        needed,
        &format!("{section}.provider"),
        one_of(AiProvider::CHOICES),
    );
    let model = r.required_if(needed, &format!("{section}.model"), string);
    let needs_key = needed && provider != Some(AiProvider::Mock);
    let api_key = r.required_if(needs_key, &format!("{section}.apiKey"), secret);
    if !needed || (needs_key && api_key.is_none()) {
        return None;
    }
    Some(AiEndpoint {
        provider: provider?,
        model: model?,
        api_key,
    })
}

fn database(r: &mut Reader) -> Option<DatabaseConfig> {
    let host = r.required("database.host", string);
    let port = r.optional("database.port", 5432, port);
    let name = r.required("database.name", string);
    let user = r.required("database.user", string);
    let password = r.required("database.password", secret);
    let ssl_mode = r.optional("database.sslMode", PgSslMode::Prefer, one_of(SSL_MODES));
    let max_connections = r.optional("database.maxConnections", 10, at_least(1));
    Some(DatabaseConfig {
        host: host?,
        port,
        name: name?,
        user: user?,
        password: password?,
        ssl_mode,
        max_connections,
    })
}

fn payment_methods(value: &Value) -> Result<Vec<PaymentMethod>, String> {
    let methods = non_empty(list(one_of(PaymentMethod::CHOICES)))(value)?;
    let repeated = methods
        .iter()
        .enumerate()
        .any(|(i, method)| methods[..i].contains(method));
    if repeated {
        return Err("must not list a method twice".to_owned());
    }
    Ok(methods)
}

fn notification(r: &mut Reader) -> NotificationConfig {
    let at = |hour, minute| NaiveTime::from_hms_opt(hour, minute, 0).expect("a valid time");
    let config = NotificationConfig {
        new_order_to_seller: r.optional("notification.newOrderToSeller", true, boolean),
        order_status_to_buyer: r.optional("notification.orderStatusToBuyer", true, boolean),
        admin_alerts: r.optional("notification.adminAlerts", true, boolean),
        daily_summary: r.optional("notification.dailySummary", false, boolean),
        daily_summary_time: r.optional("notification.dailySummaryTime", at(20, 0), time_of_day),
        group_digest: r.optional("notification.groupDigest", false, boolean),
        group_digest_time: r.optional("notification.groupDigestTime", at(7, 0), time_of_day),
    };
    // The digest has nowhere to go without the group (FR-NOT-07)
    if config.group_digest && !r.is_set("line.villageGroupId") {
        r.problem(
            "line.villageGroupId",
            "required when notification.groupDigest is true",
        );
    }
    config
}

fn privacy(r: &mut Reader) -> Option<PrivacyConfig> {
    let notice_version = r.required("privacy.noticeVersion", string);
    let notice_text = r.required("privacy.noticeText", string);
    let notice_url = r.maybe("privacy.noticeUrl", web_url);
    Some(PrivacyConfig {
        notice_version: notice_version?,
        notice_text: notice_text?,
        notice_url,
    })
}

fn retention(r: &mut Reader) -> RetentionConfig {
    RetentionConfig {
        webhook_event_days: r.optional("retention.webhookEventDays", 14, at_least(1)),
        ai_message_days: r.optional("retention.aiMessageDays", 30, at_least(1)),
        ai_call_log_days: r.optional("retention.aiCallLogDays", 90, at_least(1)),
        push_log_days: r.optional("retention.pushLogDays", 90, at_least(1)),
        slip_days: r.optional("retention.slipDays", 90, at_least(1)),
        deleted_image_days: r.optional("retention.deletedImageDays", 30, at_least(1)),
        audit_log_days: r.optional("retention.auditLogDays", 365, at_least(1)),
    }
}
