//! Tool definitions advertised by the server.
//!
//! The JSON in `assets/` is the exact `tools/list` payload produced by the
//! TypeScript implementation, so descriptions, annotations, and input/output
//! schemas are identical across both packages.

use serde_json::Value;
use std::sync::OnceLock;

/// Privacy mode (`WHOOP_MCP_PRIVACY_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrivacyMode {
    #[default]
    Standard,
    Aggregate,
}

impl PrivacyMode {
    /// Parse `standard` / `aggregate`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "standard" => Some(Self::Standard),
            "aggregate" => Some(Self::Aggregate),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Aggregate => "aggregate",
        }
    }
}

fn parse(raw: &'static str) -> Vec<Value> {
    match serde_json::from_str(raw) {
        Ok(Value::Array(tools)) => tools,
        _ => Vec::new(),
    }
}

/// Tool definitions for a privacy mode, in registration order.
pub fn tools(mode: PrivacyMode) -> &'static [Value] {
    static STANDARD: OnceLock<Vec<Value>> = OnceLock::new();
    static AGGREGATE: OnceLock<Vec<Value>> = OnceLock::new();
    match mode {
        PrivacyMode::Standard => {
            STANDARD.get_or_init(|| parse(include_str!("../assets/tools-standard.json")))
        }
        PrivacyMode::Aggregate => {
            AGGREGATE.get_or_init(|| parse(include_str!("../assets/tools-aggregate.json")))
        }
    }
}

/// Definition of one tool.
pub fn tool(mode: PrivacyMode, name: &str) -> Option<&'static Value> {
    tools(mode)
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
}

/// WHOOP record kinds with validation schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Recovery,
    Sleep,
    Cycle,
    Workout,
}

/// Validation schema for one WHOOP record (shared with the output contracts).
pub fn record_schema(kind: RecordKind) -> &'static Value {
    static SCHEMAS: OnceLock<[Value; 4]> = OnceLock::new();
    let schemas = SCHEMAS.get_or_init(|| {
        let output = |name: &str| {
            tool(PrivacyMode::Standard, name)
                .and_then(|t| t.get("outputSchema"))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let recovery = output("get_recovery_collection")
            .pointer("/properties/records/items")
            .cloned()
            .unwrap_or(Value::Null);
        [
            recovery,
            output("get_sleep_by_id"),
            output("get_cycle_by_id"),
            output("get_workout_by_id"),
        ]
    });
    match kind {
        RecordKind::Recovery => &schemas[0],
        RecordKind::Sleep => &schemas[1],
        RecordKind::Cycle => &schemas[2],
        RecordKind::Workout => &schemas[3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_patterns(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(p)) = map.get("pattern") {
                    out.push(p.clone());
                }
                map.values().for_each(|v| collect_patterns(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| collect_patterns(v, out)),
            _ => {}
        }
    }

    #[test]
    fn catalogs_load_with_expected_tools() {
        assert_eq!(tools(PrivacyMode::Standard).len(), 16);
        assert_eq!(tools(PrivacyMode::Aggregate).len(), 5);
        for kind in [
            RecordKind::Recovery,
            RecordKind::Sleep,
            RecordKind::Cycle,
            RecordKind::Workout,
        ] {
            assert_eq!(record_schema(kind)["type"], "object");
        }
    }

    #[test]
    fn every_advertised_pattern_is_supported() {
        let mut patterns = Vec::new();
        for mode in [PrivacyMode::Standard, PrivacyMode::Aggregate] {
            tools(mode)
                .iter()
                .for_each(|t| collect_patterns(t, &mut patterns));
        }
        assert!(!patterns.is_empty());
        for pattern in patterns {
            assert!(
                crate::schema::is_known_pattern(&pattern),
                "unsupported pattern {pattern}"
            );
        }
    }
}
