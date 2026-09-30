//! Structured logger writing JSON lines (or a pretty format) to stderr.
//!
//! stdout is reserved for the MCP stdio transport.

use crate::js;
use serde_json::{Map, Value};
use std::io::Write;

/// Log level, ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Parse a configured level name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

const SENSITIVE_KEYS: [&str; 10] = [
    "token",
    "accessToken",
    "access_token",
    "refreshToken",
    "refresh_token",
    "secret",
    "clientSecret",
    "client_secret",
    "password",
    "authorization",
];

/// Structured logger.
#[derive(Debug, Clone)]
pub struct Logger {
    level: LogLevel,
    pretty: bool,
    base: Vec<(String, Value)>,
}

impl Logger {
    /// Create a logger with a minimum level and output format.
    pub fn new(level: LogLevel, pretty: bool) -> Self {
        Self {
            level,
            pretty,
            base: Vec::new(),
        }
    }

    /// Create a request-scoped logger that includes `requestId` in every entry.
    pub fn with_request_id(&self, request_id: &str) -> Self {
        let mut child = self.clone();
        child
            .base
            .push(("requestId".into(), Value::from(request_id)));
        child
    }

    pub fn debug(&self, msg: &str, extra: &[(&str, Value)]) {
        self.log(LogLevel::Debug, msg, extra);
    }

    pub fn info(&self, msg: &str, extra: &[(&str, Value)]) {
        self.log(LogLevel::Info, msg, extra);
    }

    pub fn warn(&self, msg: &str, extra: &[(&str, Value)]) {
        self.log(LogLevel::Warn, msg, extra);
    }

    pub fn error(&self, msg: &str, extra: &[(&str, Value)]) {
        self.log(LogLevel::Error, msg, extra);
    }

    /// Render an entry without writing it (`None` when filtered by level).
    pub fn format(&self, level: LogLevel, msg: &str, extra: &[(&str, Value)]) -> Option<String> {
        if level < self.level {
            return None;
        }
        let mut entry = Map::new();
        entry.insert("ts".into(), Value::from(now_iso()));
        entry.insert("level".into(), Value::from(level.as_str()));
        entry.insert("msg".into(), Value::from(msg));
        let merged = self
            .base
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .chain(extra.iter().map(|(k, v)| (*k, v)));
        for (key, value) in merged {
            let value = if SENSITIVE_KEYS.contains(&key) {
                Value::from("[REDACTED]")
            } else {
                value.clone()
            };
            entry.insert(key.to_string(), value);
        }
        if !self.pretty {
            return Some(format!("{}\n", js::stringify(&Value::Object(entry))));
        }
        let mut line = format!(
            "[{}] {:<5} {msg}",
            entry["ts"].as_str().unwrap_or(""),
            level.as_str().to_uppercase()
        );
        let rest: Vec<String> = entry
            .iter()
            .skip(3)
            .map(|(k, v)| format!("{k}={}", display_value(v)))
            .collect();
        if !rest.is_empty() {
            line.push_str(&format!(" {{{}}}", rest.join(", ")));
        }
        Some(format!("{line}\n"))
    }

    fn log(&self, level: LogLevel, msg: &str, extra: &[(&str, Value)]) {
        if let Some(line) = self.format(level, msg, extra) {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        }
    }
}

/// `String(value)` for JSON values.
fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n
            .as_f64()
            .map_or_else(|| n.to_string(), js::number_to_string),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(items) => items
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(","),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
    }
}

fn now_iso() -> String {
    js::to_iso(now_ms()).unwrap_or_default()
}

/// Current wall-clock time in epoch milliseconds.
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_fields_and_filters_levels() {
        let logger = Logger::new(LogLevel::Info, false).with_request_id("r1");
        assert!(logger.format(LogLevel::Debug, "hidden", &[]).is_none());
        let line = logger
            .format(
                LogLevel::Warn,
                "hello",
                &[("token", Value::from("abc")), ("n", Value::from(1.0))],
            )
            .unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["token"], "[REDACTED]");
        assert_eq!(parsed["requestId"], "r1");
        assert_eq!(parsed["level"], "warn");
        assert!(line.contains("\"n\":1"));
    }

    #[test]
    fn pretty_format_lists_extras() {
        let logger = Logger::new(LogLevel::Debug, true);
        let line = logger
            .format(LogLevel::Info, "msg", &[("port", Value::from(3000))])
            .unwrap();
        assert!(line.contains("INFO  msg {port=3000}"));
    }
}
