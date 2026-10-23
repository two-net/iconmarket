//! Reads the parsed file key by key, collecting every problem instead of stopping at the
//! first (CFG-05), and remembering which keys it looked up so the rest can be reported as
//! unknown (CFG-07).
//!
//! Problems name the key and what was expected, never the value, which may be a secret
//! (CFG-06). An empty string counts as an absent key.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use chrono::NaiveTime;
use chrono_tz::Tz;
use toml::{Table, Value};
use url::Url;

use super::Secret;

pub struct Reader<'a> {
    root: &'a Table,
    problems: Vec<String>,
    /// Sections and keys looked up so far, whether or not the file has them
    sections: BTreeSet<String>,
    keys: BTreeSet<String>,
    defaulted: Vec<String>,
}

/// What [`Reader::finish`] found.
pub struct Report {
    /// `key: expectation`, in reading order
    pub problems: Vec<String>,
    /// Keys in the file that nothing looked up
    pub unknown_keys: Vec<String>,
    /// Optional keys absent from the file, which took their defaults
    pub defaulted: Vec<String>,
}

enum Lookup<'a> {
    Present(&'a Value),
    /// Absent, or an empty string
    Unset {
        absent: bool,
    },
    /// A section on the way isn't a table, which has been reported
    Unreachable,
}

impl<'a> Reader<'a> {
    pub fn new(root: &'a Table) -> Self {
        Self {
            root,
            problems: Vec::new(),
            sections: BTreeSet::new(),
            keys: BTreeSet::new(),
            defaulted: Vec::new(),
        }
    }

    /// A key that must be set. `None` means a problem has been recorded.
    pub fn required<T>(
        &mut self,
        path: &str,
        convert: impl FnOnce(&Value) -> Result<T, String>,
    ) -> Option<T> {
        self.required_if(true, path, convert)
    }

    /// A key that must be set when `needed`, and may be left out otherwise.
    pub fn required_if<T>(
        &mut self,
        needed: bool,
        path: &str,
        convert: impl FnOnce(&Value) -> Result<T, String>,
    ) -> Option<T> {
        match self.lookup(path) {
            Lookup::Present(value) => convert(value)
                .map_err(|message| self.problem(path, message))
                .ok(),
            Lookup::Unset { .. } if needed => {
                self.problem(path, "required");
                None
            }
            Lookup::Unset { absent } => {
                if absent {
                    self.defaulted.push(path.to_owned());
                }
                None
            }
            Lookup::Unreachable => None,
        }
    }

    /// A key that may be left out, meaning "off" or "none".
    pub fn maybe<T>(
        &mut self,
        path: &str,
        convert: impl FnOnce(&Value) -> Result<T, String>,
    ) -> Option<T> {
        self.required_if(false, path, convert)
    }

    /// A key that takes `default` when left out.
    pub fn optional<T>(
        &mut self,
        path: &str,
        default: T,
        convert: impl FnOnce(&Value) -> Result<T, String>,
    ) -> T {
        self.maybe(path, convert).unwrap_or(default)
    }

    /// Whether the file sets `path`, valid or not, without reading it.
    pub fn is_set(&self, path: &str) -> bool {
        let mut table = self.root;
        let mut names = path.split('.').peekable();
        while let Some(name) = names.next() {
            match (table.get(name), names.peek()) {
                (Some(Value::Table(inner)), Some(_)) => table = inner,
                (Some(Value::String(text)), None) => return !text.trim().is_empty(),
                (Some(_), None) => return true,
                _ => return false,
            }
        }
        false
    }

    /// Records a problem a cross-key rule found.
    pub fn problem(&mut self, path: &str, message: impl std::fmt::Display) {
        self.problems.push(format!("{path}: {message}"));
    }

    pub fn finish(self) -> Report {
        let mut unknown_keys = Vec::new();
        self.collect_unknown(self.root, "", &mut unknown_keys);
        Report {
            problems: self.problems,
            unknown_keys,
            defaulted: self.defaulted,
        }
    }

    fn lookup(&mut self, path: &str) -> Lookup<'a> {
        self.keys.insert(path.to_owned());
        let (sections, key) = path.rsplit_once('.').expect("every key is in a section");

        let mut table = self.root;
        let mut section = String::new();
        for name in sections.split('.') {
            if !section.is_empty() {
                section.push('.');
            }
            section.push_str(name);
            let first_visit = self.sections.insert(section.clone());
            match table.get(name) {
                Some(Value::Table(inner)) => table = inner,
                Some(value) => {
                    if first_visit {
                        self.problem(&section, expected("a table", value));
                    }
                    return Lookup::Unreachable;
                }
                None => return Lookup::Unset { absent: true },
            }
        }

        match table.get(key) {
            Some(Value::String(text)) if text.trim().is_empty() => Lookup::Unset { absent: false },
            Some(value) => Lookup::Present(value),
            None => Lookup::Unset { absent: true },
        }
    }

    fn collect_unknown(&self, table: &Table, prefix: &str, unknown: &mut Vec<String>) {
        for (name, value) in table {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if self.sections.contains(&path) {
                if let Value::Table(inner) = value {
                    self.collect_unknown(inner, &path, unknown);
                }
            } else if !self.keys.contains(&path) {
                unknown.push(path);
            }
        }
    }
}

/// "expected `what`, found a list": names the value's type, never the value.
fn expected(what: &str, value: &Value) -> String {
    let found = match value {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a decimal number",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a date or time",
        Value::Array(_) => "a list",
        Value::Table(_) => "a table",
    };
    format!("expected {what}, found {found}")
}

/// A non-empty string.
pub fn string(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) if text.trim().is_empty() => Err("must not be empty".to_owned()),
        Value::String(text) => Ok(text.clone()),
        _ => Err(expected("a quoted string", value)),
    }
}

pub fn secret(value: &Value) -> Result<Secret, String> {
    string(value).map(Secret)
}

pub fn boolean(value: &Value) -> Result<bool, String> {
    match *value {
        Value::Boolean(flag) => Ok(flag),
        _ => Err(expected("true or false", value)),
    }
}

pub fn port(value: &Value) -> Result<u16, String> {
    let Value::Integer(n) = *value else {
        return Err(expected("an integer", value));
    };
    u16::try_from(n)
        .ok()
        .filter(|&port| port != 0)
        .ok_or_else(|| "must be between 1 and 65535".to_owned())
}

pub fn at_least(min: u32) -> impl Fn(&Value) -> Result<u32, String> {
    move |value| {
        let Value::Integer(n) = *value else {
            return Err(expected("an integer", value));
        };
        if n < i64::from(min) {
            return Err(format!("must be at least {min}"));
        }
        u32::try_from(n).map_err(|_| format!("must be at most {}", u32::MAX))
    }
}

/// A number in `range`; integers are accepted too.
pub fn number(range: RangeInclusive<f64>) -> impl Fn(&Value) -> Result<f64, String> {
    move |value| {
        let n = match *value {
            Value::Float(n) => n,
            Value::Integer(n) => n as f64,
            _ => return Err(expected("a number", value)),
        };
        if range.contains(&n) {
            Ok(n)
        } else {
            Err(format!(
                "must be between {} and {}",
                range.start(),
                range.end()
            ))
        }
    }
}

/// One of the `choices`, by name.
pub fn one_of<T: Copy>(
    choices: &'static [(&'static str, T)],
) -> impl Fn(&Value) -> Result<T, String> {
    move |value| {
        let text = string(value)?;
        choices
            .iter()
            .find(|(name, _)| *name == text)
            .map(|&(_, choice)| choice)
            .ok_or_else(|| {
                let names: Vec<_> = choices.iter().map(|(name, _)| *name).collect();
                format!("must be one of {}", names.join(", "))
            })
    }
}

/// A list whose items each pass `item`. Every bad item is reported, by position.
pub fn list<T>(
    item: impl Fn(&Value) -> Result<T, String>,
) -> impl Fn(&Value) -> Result<Vec<T>, String> {
    move |value| {
        let Value::Array(values) = value else {
            return Err(expected("a list", value));
        };
        let mut items = Vec::with_capacity(values.len());
        let mut problems = Vec::new();
        for (i, value) in values.iter().enumerate() {
            match item(value) {
                Ok(converted) => items.push(converted),
                Err(message) => problems.push(format!("item {}: {message}", i + 1)),
            }
        }
        if problems.is_empty() {
            Ok(items)
        } else {
            Err(problems.join("; "))
        }
    }
}

/// Like `convert`, but an empty list is a problem.
pub fn non_empty<T>(
    convert: impl Fn(&Value) -> Result<Vec<T>, String>,
) -> impl Fn(&Value) -> Result<Vec<T>, String> {
    move |value| {
        let items = convert(value)?;
        if items.is_empty() {
            Err("must have at least one item".to_owned())
        } else {
            Ok(items)
        }
    }
}

pub fn https_url(value: &Value) -> Result<Url, String> {
    url_with_scheme(value, &["https"], "must be an https:// URL")
}

pub fn web_url(value: &Value) -> Result<Url, String> {
    url_with_scheme(
        value,
        &["http", "https"],
        "must be an http:// or https:// URL",
    )
}

fn url_with_scheme(value: &Value, schemes: &[&str], message: &str) -> Result<Url, String> {
    let text = string(value)?;
    match Url::parse(&text) {
        Ok(url) if schemes.contains(&url.scheme()) && url.host().is_some() => Ok(url),
        _ => Err(message.to_owned()),
    }
}

/// An IANA time zone name, checked against the database compiled into chrono-tz.
pub fn time_zone(value: &Value) -> Result<Tz, String> {
    string(value)?
        .parse()
        .map_err(|_| "must be an IANA time zone name such as Asia/Bangkok".to_owned())
}

/// A time of day written `HH:mm`.
pub fn time_of_day(value: &Value) -> Result<NaiveTime, String> {
    let text = string(value)?;
    let shaped = text.len() == 5 && text.as_bytes()[2] == b':';
    shaped
        .then(|| NaiveTime::parse_from_str(&text, "%H:%M").ok())
        .flatten()
        .ok_or_else(|| "must be a time written HH:mm, such as 07:00".to_owned())
}

/// A LINE user (`U`) or group (`C`) ID: the prefix and 32 lowercase hex digits.
pub fn line_id(prefix: char, kind: &'static str) -> impl Fn(&Value) -> Result<String, String> {
    move |value| {
        let id = string(value)?;
        let valid = id.len() == 33
            && id.starts_with(prefix)
            && id[1..]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(id)
        } else {
            Err(format!(
                "must be a LINE {kind} ID: {prefix} followed by 32 lowercase hex digits"
            ))
        }
    }
}
