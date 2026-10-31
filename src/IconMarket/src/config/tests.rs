use super::*;

const SAMPLE: &str = include_str!("../../iconmarket.sample.toml");

/// Only the required keys.
const MINIMAL: &str = r#"
[app]
baseUrl = "https://market.example.com"

[village]
name = "Test village"

[line]
channelSecret = "line-secret-value"
channelAccessToken = "line-token-value"

[ai]
provider = "claude"
model = "a-model"
apiKey = "ai-key-value"

[database]
host = "localhost"
name = "iconmarket"
user = "iconmarket"
password = "db-password-value"

[privacy]
noticeVersion = "1"
noticeText = "Notice"

[admin]
lineUserIds = ["U0123456789abcdef0123456789abcdef"]
"#;

/// The secrets of MINIMAL, plus the fallback key some tests add.
const SECRETS: [&str; 5] = [
    "line-secret-value",
    "line-token-value",
    "ai-key-value",
    "fallback-key-value",
    "db-password-value",
];

const FALLBACK: &str = r#"
[ai.fallback]
provider = "gemini"
model = "b-model"
apiKey = "fallback-key-value"
"#;

fn valid(text: &str) -> Loaded {
    load_str(text).unwrap_or_else(|invalid| panic!("{invalid}"))
}

fn problems(text: &str) -> Vec<String> {
    match load_str(text) {
        Ok(_) => panic!("expected problems"),
        Err(invalid) => invalid.problems,
    }
}

fn assert_no_secrets(output: &str) {
    for secret in SECRETS {
        assert!(!output.contains(secret), "{output}");
    }
}

#[test]
fn sample_file_is_valid_and_lists_every_key() {
    let table = parse(SAMPLE).unwrap();
    let (config, report) = read(&table);
    assert_eq!(report.problems, Vec::<String>::new());
    assert!(config.is_some());
    assert_eq!(report.unknown_keys, Vec::<String>::new());
    assert_eq!(
        report.defaulted,
        Vec::<String>::new(),
        "missing from the sample"
    );
}

#[test]
fn defaults_follow_the_srs() {
    let Loaded {
        config,
        unknown_keys,
    } = valid(MINIMAL);
    assert!(unknown_keys.is_empty());

    let app = &config.app;
    assert_eq!(app.name, "IconMarket");
    assert_eq!(app.listen_port, 8080);
    assert_eq!(app.time_zone, chrono_tz::Asia::Bangkok);
    assert_eq!(app.polite_particle, PoliteParticle::Kha);

    let village = &config.village;
    assert!(village.zones.is_empty());
    assert_eq!(village.join_code, None);
    assert_eq!(village.admin_contact, None);

    assert!(config.line.rich_menu.auto_provision);
    assert_eq!(config.line.village_group_id, None);
    assert_eq!(config.line.api_base_url, None);
    assert_eq!(config.line.data_base_url, None);

    let ai = &config.ai;
    let primary = ai.primary.as_ref().unwrap();
    assert_eq!(primary.provider, AiProvider::Claude);
    assert_eq!(primary.model, "a-model");
    assert_eq!(ai.base_url, None);
    assert_eq!(ai.timeout_seconds, 20);
    assert_eq!(ai.max_output_tokens, 1024);
    assert!((ai.temperature - 0.3).abs() < f64::EPSILON);
    assert_eq!(ai.history_turns, 10);
    let features = &ai.features;
    assert!(features.assistant && features.listing && features.moderation);
    assert!(!features.slip_reading && !features.voice && !features.visual_search);
    assert_eq!(ai.limits.per_user_daily_calls, 50);
    assert_eq!(ai.limits.global_daily_calls, 2000);
    assert!(!ai.log_content);
    assert!(ai.fallback.is_none());

    let database = &config.database;
    assert_eq!(database.port, 5432);
    assert!(matches!(database.ssl_mode, PgSslMode::Prefer));
    assert_eq!(database.max_connections, 10);

    assert!(config.shop.require_approval);
    assert_eq!(config.order.pending_timeout_minutes, 60);
    assert_eq!(
        config.payment.methods,
        [PaymentMethod::Cash, PaymentMethod::PromptPay]
    );

    let notification = &config.notification;
    assert!(notification.new_order_to_seller);
    assert!(notification.order_status_to_buyer);
    assert!(notification.admin_alerts);
    assert!(!notification.daily_summary);
    assert_eq!(notification.daily_summary_time.to_string(), "20:00:00");
    assert!(!notification.group_digest);
    assert_eq!(notification.group_digest_time.to_string(), "07:00:00");

    let image = &config.image;
    assert_eq!(image.max_upload_mb, 10);
    assert_eq!(image.max_dimension_px, 1024);
    assert_eq!(image.max_per_product, 3);
    assert_eq!(image.slip_link_hours, 24);

    assert_eq!(config.session.timeout_minutes, 15);
    assert_eq!(config.session.pending_action_minutes, 60);
    assert_eq!(config.limits.messages_per_user_per_minute, 20);

    assert_eq!(config.catalog.default_categories, DEFAULT_CATEGORIES);
    assert!(config.catalog.prohibited_keywords.is_empty());
    assert_eq!(config.privacy.notice_url, None);

    assert_eq!(config.logging.level, LogLevel::Info);
    assert!(config.logging.file.as_os_str().is_empty());

    let retention = &config.retention;
    assert_eq!(retention.webhook_event_days, 14);
    assert_eq!(retention.ai_message_days, 30);
    assert_eq!(retention.ai_call_log_days, 90);
    assert_eq!(retention.push_log_days, 90);
    assert_eq!(retention.slip_days, 90);
    assert_eq!(retention.deleted_image_days, 30);
    assert_eq!(retention.audit_log_days, 365);
}

#[test]
fn lines_hosts_must_be_web_urls() {
    let text = MINIMAL.replace(
        "[ai]",
        "apiBaseUrl = \"http://127.0.0.1:9001\"\ndataBaseUrl = \"ftp://127.0.0.1\"\n\n[ai]",
    );
    assert_eq!(
        problems(&text),
        ["line.dataBaseUrl: must be an http:// or https:// URL"]
    );

    let text = text.replace("ftp://127.0.0.1", "http://127.0.0.1:9002");
    let line = valid(&text).config.line;
    assert_eq!(
        line.api_base_url.unwrap().as_str(),
        "http://127.0.0.1:9001/"
    );
    assert_eq!(
        line.data_base_url.unwrap().as_str(),
        "http://127.0.0.1:9002/"
    );
}

/// AC-09
#[test]
fn missing_password_is_named_and_no_secret_is_printed() {
    let text = MINIMAL.replace(r#"password = "db-password-value""#, "");
    let invalid = load_str(&text).unwrap_err();
    assert_eq!(invalid.problems, ["database.password: required"]);
    assert_no_secrets(&invalid.to_string());
}

/// The privacy notice must fit one LINE card, and its link a LINE button.
#[test]
fn the_privacy_notice_fits_a_card() {
    let notice = |text: &str, url: &str| {
        let privacy = format!(
            "[privacy]\nnoticeVersion = \"1\"\nnoticeText = \"{text}\"\nnoticeUrl = \"{url}\"\n"
        );
        MINIMAL.replace(
            "[privacy]\nnoticeVersion = \"1\"\nnoticeText = \"Notice\"\n",
            &privacy,
        )
    };
    let longest = "ข".repeat(NOTICE_TEXT_CHARS);
    let url = format!("https://market.example.com/{}", "p".repeat(URI_CHARS - 27));
    let privacy = valid(&notice(&longest, &url)).config.privacy;
    assert_eq!(privacy.notice_text, longest);
    assert_eq!(privacy.notice_url.unwrap().as_str(), url);

    assert_eq!(
        problems(&notice(&format!("{longest}ข"), &format!("{url}p"))),
        [
            "privacy.noticeText: must be at most 3000 characters",
            "privacy.noticeUrl: must be at most 1000 characters",
        ]
    );
}

#[test]
fn every_problem_is_reported_in_one_run() {
    let text = r#"
[app]
baseUrl = "http://market.example.com"
listenPort = 0
timeZone = "Asia/Bankok"

[village]

[line]
channelSecret = "line-secret-value"
channelAccessToken = "line-token-value"

[ai]
provider = "chatgpt"
model = "a-model"
apiKey = "ai-key-value"
temperature = 3

[database]
host = "localhost"
name = "iconmarket"
user = "iconmarket"
maxConnections = "ten"

[payment]
methods = ["cash", "card"]

[notification]
dailySummaryTime = "8pm"

[privacy]
noticeVersion = "1"
noticeText = "Notice"

[admin]
lineUserIds = []

[logging]
level = "verbose"
"#;
    assert_eq!(
        problems(text),
        [
            "app.baseUrl: must be an https:// URL",
            "app.listenPort: must be between 1 and 65535",
            "app.timeZone: must be an IANA time zone name such as Asia/Bangkok",
            "village.name: required",
            "ai.provider: must be one of claude, gemini, gpt, grok, mock",
            "ai.temperature: must be between 0 and 2",
            "database.password: required",
            "database.maxConnections: expected an integer, found a string",
            "payment.methods: item 2: must be one of cash, promptpay",
            "notification.dailySummaryTime: must be a time written HH:mm, such as 07:00",
            "admin.lineUserIds: must have at least one item",
            "logging.level: must be one of debug, info, warn, error",
        ]
    );
}

#[test]
fn an_empty_string_counts_as_absent() {
    let text = MINIMAL
        .replace(r#"password = "db-password-value""#, r#"password = """#)
        .replace("[app]", "[app]\nname = \"  \"");
    assert_eq!(problems(&text), ["database.password: required"]);

    let text = MINIMAL.replace("[app]", "[app]\nname = \"\"");
    assert_eq!(valid(&text).config.app.name, "IconMarket");
}

#[test]
fn ai_keys_are_required_only_when_enabled() {
    let without_ai = MINIMAL
        .replace(r#"provider = "claude""#, "")
        .replace(r#"model = "a-model""#, "")
        .replace(r#"apiKey = "ai-key-value""#, "");
    assert_eq!(
        problems(&without_ai),
        [
            "ai.provider: required",
            "ai.model: required",
            "ai.apiKey: required",
        ]
    );

    let disabled = without_ai.replace("[ai]", "[ai]\nenabled = false");
    assert!(valid(&disabled).config.ai.primary.is_none());

    // Keys of a disabled AI are still checked, and aren't unknown
    let disabled = MINIMAL.replace("[ai]", "[ai]\nenabled = false");
    let loaded = valid(&disabled);
    assert!(loaded.config.ai.primary.is_none());
    assert!(loaded.unknown_keys.is_empty());
}

#[test]
fn the_mock_provider_needs_no_key() {
    let text = MINIMAL
        .replace(r#"provider = "claude""#, r#"provider = "mock""#)
        .replace(r#"apiKey = "ai-key-value""#, "");
    let primary = valid(&text).config.ai.primary.unwrap();
    assert_eq!(primary.provider, AiProvider::Mock);
    assert!(primary.api_key.is_none());
}

#[test]
fn a_fallback_provider_needs_its_model_and_key() {
    let text = format!("{MINIMAL}{FALLBACK}");
    let fallback = valid(&text).config.ai.fallback.unwrap();
    assert_eq!(fallback.provider, AiProvider::Gemini);
    assert_eq!(fallback.model, "b-model");
    assert_eq!(fallback.api_key.unwrap().expose(), "fallback-key-value");

    let text = format!("{MINIMAL}[ai.fallback]\nprovider = \"gemini\"\nmodel = \"\"\n");
    assert_eq!(
        problems(&text),
        [
            "ai.fallback.model: required",
            "ai.fallback.apiKey: required"
        ]
    );

    let text = format!("{MINIMAL}[ai.fallback]\nprovider = \"mock\"\nmodel = \"mock\"\n");
    let fallback = valid(&text).config.ai.fallback.unwrap();
    assert!(fallback.api_key.is_none());
}

#[test]
fn the_group_digest_needs_the_group_id() {
    let text = format!("{MINIMAL}[notification]\ngroupDigest = true\n");
    assert_eq!(
        problems(&text),
        ["line.villageGroupId: required when notification.groupDigest is true"]
    );

    let with_group = text.replace(
        "[line]",
        "[line]\nvillageGroupId = \"C0123456789abcdef0123456789abcdef\"",
    );
    let config = valid(&with_group).config;
    assert!(config.notification.group_digest);
    assert_eq!(
        config.line.village_group_id.as_deref(),
        Some("C0123456789abcdef0123456789abcdef")
    );
}

#[test]
fn line_ids_are_checked_item_by_item() {
    let text = MINIMAL
        .replace(
            r#"lineUserIds = ["U0123456789abcdef0123456789abcdef"]"#,
            r#"lineUserIds = ["U0123456789abcdef0123456789abcdef", "Somchai", "U0123456789ABCDEF0123456789ABCDEF"]"#,
        )
        .replace("[line]", "[line]\nvillageGroupId = \"U0123456789abcdef0123456789abcdef\"");
    assert_eq!(
        problems(&text),
        [
            "line.villageGroupId: must be a LINE group ID: C followed by 32 lowercase hex digits",
            "admin.lineUserIds: item 2: must be a LINE user ID: U followed by 32 lowercase hex digits; \
             item 3: must be a LINE user ID: U followed by 32 lowercase hex digits",
        ]
    );
}

#[test]
fn payment_methods_are_not_repeated() {
    let text = format!("{MINIMAL}[payment]\nmethods = [\"cash\", \"cash\"]\n");
    assert_eq!(
        problems(&text),
        ["payment.methods: must not list a method twice"]
    );
}

#[test]
fn unknown_keys_are_warnings_not_problems() {
    let text = format!(
        "foo = 1\n{}\n[databse]\npassword = \"typo-value\"\n[ai.features]\nvoices = true\n",
        MINIMAL.replace("[app]", "[app]\nlistenPorts = 8080")
    );
    let loaded = valid(&text);
    assert_eq!(
        loaded.unknown_keys,
        ["ai.features.voices", "app.listenPorts", "databse", "foo"]
    );
}

#[test]
fn a_section_that_is_not_a_table_is_reported_once() {
    let text = format!(
        "ai = 5\n{}",
        MINIMAL
            .replace("[ai]", "")
            .replace(r#"provider = "claude""#, "")
            .replace(r#"model = "a-model""#, "")
            .replace(r#"apiKey = "ai-key-value""#, "")
    );
    assert_eq!(problems(&text), ["ai: expected a table, found an integer"]);
}

#[test]
fn debug_output_masks_every_secret() {
    let config = valid(&format!("{MINIMAL}{FALLBACK}")).config;
    let debug = format!("{config:?}");
    assert_no_secrets(&debug);
    assert_eq!(debug.matches("\"***\"").count(), 5, "{debug}");
}

#[test]
fn secrets_of_the_wrong_type_are_named_but_not_printed() {
    let mut text = format!("{MINIMAL}{FALLBACK}");
    for secret in SECRETS {
        text = text.replace(&format!("\"{secret}\""), "98765432");
    }
    let invalid = load_str(&text).unwrap_err();
    assert_eq!(
        invalid.problems,
        [
            "line.channelSecret: expected a quoted string, found an integer",
            "line.channelAccessToken: expected a quoted string, found an integer",
            "ai.apiKey: expected a quoted string, found an integer",
            "ai.fallback.apiKey: expected a quoted string, found an integer",
            "database.password: expected a quoted string, found an integer",
        ]
    );
    assert!(!invalid.to_string().contains("98765432"), "{invalid}");
}

#[test]
fn syntax_errors_give_the_position_without_the_line() {
    let text = MINIMAL.replace(
        r#"password = "db-password-value""#,
        "password = db-password-value",
    );
    let line = text
        .lines()
        .position(|l| l.starts_with("password"))
        .unwrap()
        + 1;
    let problems = problems(&text);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&format!("(line {line}, column ")),
        "{problems:?}"
    );
    assert_no_secrets(&problems[0]);
}

#[test]
fn first_argument_overrides_the_default_path() {
    assert_eq!(
        path_from_args(["icon-market".into()]),
        PathBuf::from(DEFAULT_PATH)
    );
    assert_eq!(
        path_from_args(["icon-market".into(), "/etc/iconmarket.toml".into()]),
        PathBuf::from("/etc/iconmarket.toml")
    );
}
