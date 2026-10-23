//! The configuration file: the only source of configuration (CON-08, CFG-01 – CFG-03).
//!
//! Only the keys the app skeleton reads are modelled so far; #2 adds the rest of §6.2,
//! full validation and unknown-key warnings. Key names follow §6.2 (`app.listenPort`).

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use sqlx::postgres::PgSslMode;
use tracing::level_filters::LevelFilter;

/// Read from the working directory unless the first command-line argument names another file.
pub const DEFAULT_PATH: &str = "iconmarket.toml";

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub app: AppConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub database: DatabaseConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    pub listen_port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { listen_port: 8080 }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: LogLevel,
    /// Empty means standard output.
    pub file: PathBuf,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseConfig {
    pub host: String,
    #[serde(default = "default_database_port")]
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: Secret,
    #[serde(default, deserialize_with = "deserialize_ssl_mode")]
    pub ssl_mode: PgSslMode,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_database_port() -> u16 {
    5432
}

fn default_max_connections() -> u32 {
    10
}

fn deserialize_ssl_mode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PgSslMode, D::Error> {
    let mode = String::deserialize(deserializer)?;
    mode.parse().map_err(|_| {
        D::Error::custom(format!(
            "unknown SSL mode `{mode}`, expected one of disable, allow, prefer, require, verify-ca, verify-full"
        ))
    })
}

/// A value that must never be logged, shown or sent anywhere but its destination (CFG-06).
/// `Debug` prints a mask, and parse errors never echo it.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"***\"")
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Accept any value first: a plain `String` would report a wrong type as e.g.
        // "invalid type: integer `1234`", printing the secret
        match toml::Value::deserialize(deserializer)? {
            toml::Value::String(value) => Ok(Self(value)),
            _ => Err(D::Error::custom("expected a quoted string")),
        }
    }
}

pub fn path_from_args(args: impl IntoIterator<Item = OsString>) -> PathBuf {
    args.into_iter()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_PATH), PathBuf::from)
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    parse(&text).with_context(|| format!("invalid config file {}", path.display()))
}

fn parse(text: &str) -> anyhow::Result<Config> {
    toml::from_str(text).map_err(|e| {
        // Message and position only: the full error quotes the offending line, which may
        // hold a secret
        match e.span() {
            Some(span) => {
                let (line, column) = line_column(text, span.start);
                anyhow!("{} (line {line}, column {column})", e.message())
            }
            None => anyhow!("{}", e.message()),
        }
    })
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

/// Checks serde can't express. #2 extends this to every key in §6.2.
pub fn validate(config: &Config) -> anyhow::Result<()> {
    let mut problems = Vec::new();
    if config.app.listen_port == 0 {
        problems.push("app.listenPort must be between 1 and 65535");
    }
    if config.database.max_connections == 0 {
        problems.push("database.maxConnections must be at least 1");
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../iconmarket.sample.toml");

    const MINIMAL: &str = r#"
        [database]
        host = "localhost"
        name = "iconmarket"
        user = "iconmarket"
        password = "s3cret-value"
    "#;

    #[test]
    fn sample_file_is_valid() {
        let config = parse(SAMPLE).unwrap();
        validate(&config).unwrap();
    }

    #[test]
    fn defaults_follow_the_srs() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.app.listen_port, 8080);
        assert_eq!(config.logging.level, LogLevel::Info);
        assert!(config.logging.file.as_os_str().is_empty());
        assert_eq!(config.database.port, 5432);
        assert!(matches!(config.database.ssl_mode, PgSslMode::Prefer));
        assert_eq!(config.database.max_connections, 10);
    }

    #[test]
    fn missing_key_is_named() {
        let text = MINIMAL.replace(r#"password = "s3cret-value""#, "");
        let error = parse(&text).unwrap_err().to_string();
        assert!(error.contains("missing field `password`"), "{error}");
    }

    #[test]
    fn debug_output_masks_the_password() {
        let config = parse(MINIMAL).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("s3cret-value"), "{debug}");
        assert!(debug.contains("***"), "{debug}");
    }

    #[test]
    fn parse_errors_never_echo_the_password() {
        for bad in ["password = 98765432", "password = s3cret-value"] {
            let text = MINIMAL.replace(r#"password = "s3cret-value""#, bad);
            let error = format!("{:#}", parse(&text).unwrap_err());
            assert!(!error.contains("98765432"), "{error}");
            assert!(!error.contains("s3cret-value"), "{error}");
            assert!(error.contains("line 6"), "{error}");
        }
    }

    #[test]
    fn invalid_values_are_rejected() {
        let text = format!("[logging]\nlevel = \"verbose\"\n{MINIMAL}");
        assert!(parse(&text).is_err());

        let text = MINIMAL.replace("[database]", "[database]\nsslMode = \"sometimes\"");
        let error = parse(&text).unwrap_err().to_string();
        assert!(error.contains("unknown SSL mode `sometimes`"), "{error}");
    }

    #[test]
    fn validation_reports_every_problem() {
        let text = format!("[app]\nlistenPort = 0\n{MINIMAL}")
            .replace("[database]", "[database]\nmaxConnections = 0");
        let error = validate(&parse(&text).unwrap()).unwrap_err().to_string();
        assert!(error.contains("app.listenPort"), "{error}");
        assert!(error.contains("database.maxConnections"), "{error}");
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
}
