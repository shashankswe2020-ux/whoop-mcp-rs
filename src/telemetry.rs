//! Opt-in, aggregate-only usage telemetry.
//!
//! Disabled unless `WHOOP_MCP_TELEMETRY=1` with an HTTPS endpoint, and always
//! off under `DO_NOT_TRACK=1` or non-standard privacy modes. Events carry only
//! command/tool/prompt names and outcomes — never health data, arguments, or
//! identifiers.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

const MAX_PENDING: usize = 8;
const SEND_TIMEOUT: Duration = Duration::from_millis(500);

const TOOL_NAMES: [&str; 16] = [
    "get_profile",
    "get_body_measurement",
    "get_recovery_collection",
    "get_sleep_collection",
    "get_workout_collection",
    "get_cycle_collection",
    "get_sleep_by_id",
    "get_workout_by_id",
    "get_cycle_by_id",
    "get_weekly_summary",
    "compare_periods",
    "get_trend",
    "get_today",
    "get_calendar",
    "get_baselines",
    "get_sleep_debt",
];
const PROMPT_NAMES: [&str; 5] = [
    "weekly_health_review",
    "sleep_analysis",
    "recovery_trend",
    "workout_recap",
    "health_check",
];
const ERROR_CATEGORIES: [&str; 8] = [
    "api_auth",
    "api_rate_limit",
    "api_client",
    "api_server",
    "network",
    "invalid_data",
    "output_contract",
    "unexpected",
];

/// Package version reported in events (suffixed to distinguish the Rust build).
pub fn package_version() -> String {
    format!("{}-rust", env!("CARGO_PKG_VERSION"))
}

/// A telemetry event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryEvent {
    Prompt {
        name: String,
    },
    Command {
        name: String,
        success: bool,
    },
    Tool {
        name: String,
        success: bool,
        error_category: Option<String>,
    },
}

impl TelemetryEvent {
    fn to_json(&self) -> Option<Value> {
        let outcome = |success: bool| if success { "success" } else { "error" };
        match self {
            Self::Prompt { name } if PROMPT_NAMES.contains(&name.as_str()) => {
                Some(json!({"kind": "prompt", "name": name, "outcome": "success"}))
            }
            Self::Command { name, success } if name == "serve" || name == "setup" => {
                Some(json!({"kind": "command", "name": name, "outcome": outcome(*success)}))
            }
            Self::Tool {
                name,
                success,
                error_category,
            } if TOOL_NAMES.contains(&name.as_str()) => {
                let mut event = json!({"kind": "tool", "name": name, "outcome": outcome(*success)});
                match error_category {
                    Some(category)
                        if !*success && ERROR_CATEGORIES.contains(&category.as_str()) =>
                    {
                        event["error_category"] = Value::from(category.clone());
                    }
                    Some(_) => return None,
                    None => {}
                }
                Some(event)
            }
            _ => None,
        }
    }
}

/// Telemetry sender.
pub struct Telemetry {
    endpoint: Option<String>,
    reason: &'static str,
    pending: Arc<Mutex<Vec<JoinHandle<()>>>>,
    http: Option<reqwest::Client>,
}

impl Telemetry {
    /// Build from an environment lookup.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut endpoint = None;
        let reason = if get("DO_NOT_TRACK").as_deref() == Some("1") {
            "do_not_track"
        } else if get("WHOOP_MCP_PRIVACY_MODE")
            .as_deref()
            .unwrap_or("standard")
            != "standard"
        {
            "privacy_mode"
        } else if get("WHOOP_MCP_TELEMETRY").as_deref() == Some("1") {
            let raw = get("WHOOP_MCP_TELEMETRY_ENDPOINT").unwrap_or_default();
            match url::Url::parse(&raw) {
                Ok(url)
                    if url.scheme() == "https"
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none_or(str::is_empty)
                        && url.fragment().is_none_or(str::is_empty) =>
                {
                    endpoint = Some(url.to_string());
                    "enabled"
                }
                _ => "invalid_endpoint",
            }
        } else {
            "not_opted_in"
        };
        let http = endpoint.as_ref().map(|_| crate::net::http_client(false));
        Self {
            endpoint,
            reason,
            pending: Arc::new(Mutex::new(Vec::new())),
            http,
        }
    }

    /// Build from the process environment.
    pub fn from_process_env() -> Self {
        Self::from_env(|key| std::env::var(key).ok())
    }

    /// Whether events will be sent.
    pub fn enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    /// `{ "enabled": bool, "reason": ... }`
    pub fn status(&self) -> Value {
        json!({"enabled": self.enabled(), "reason": self.reason})
    }

    /// Queue an event (dropped when disabled, invalid, or too many are pending).
    pub fn record(&self, event: &TelemetryEvent) {
        let (Some(endpoint), Some(http)) = (&self.endpoint, &self.http) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|task| !task.is_finished());
        if pending.len() >= MAX_PENDING {
            return;
        }
        let Some(fields) = event.to_json() else {
            return;
        };
        let mut body = json!({"schema_version": 1, "package_version": package_version()});
        if let (Some(body), Value::Object(fields)) = (body.as_object_mut(), fields) {
            body.extend(fields);
        }
        let request = http
            .post(endpoint)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .timeout(SEND_TIMEOUT);
        pending.push(runtime.spawn(async move {
            let _ = request.send().await;
        }));
    }

    /// Wait for queued events to finish sending.
    pub async fn flush(&self) {
        let tasks: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for task in tasks {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn telemetry(pairs: &[(&str, &str)]) -> Telemetry {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        Telemetry::from_env(|key| env.get(key).cloned())
    }

    #[test]
    fn status_reflects_consent_and_suppression() {
        assert_eq!(
            telemetry(&[]).status(),
            json!({"enabled": false, "reason": "not_opted_in"})
        );
        let endpoint = ("WHOOP_MCP_TELEMETRY_ENDPOINT", "https://example.com/events");
        assert_eq!(
            telemetry(&[("WHOOP_MCP_TELEMETRY", "1"), endpoint]).status()["reason"],
            "enabled"
        );
        assert_eq!(
            telemetry(&[
                ("WHOOP_MCP_TELEMETRY", "1"),
                endpoint,
                ("DO_NOT_TRACK", "1")
            ])
            .status()["reason"],
            "do_not_track"
        );
        assert_eq!(
            telemetry(&[
                ("WHOOP_MCP_TELEMETRY", "1"),
                endpoint,
                ("WHOOP_MCP_PRIVACY_MODE", "aggregate")
            ])
            .status()["reason"],
            "privacy_mode"
        );
        for bad in [
            "http://example.com/e",
            "https://u:p@example.com/e",
            "https://example.com/e?x=1",
            "https://example.com/e#f",
            "nope",
        ] {
            let t = telemetry(&[
                ("WHOOP_MCP_TELEMETRY", "1"),
                ("WHOOP_MCP_TELEMETRY_ENDPOINT", bad),
            ]);
            assert_eq!(
                t.status(),
                json!({"enabled": false, "reason": "invalid_endpoint"}),
                "{bad}"
            );
        }
    }

    #[test]
    fn events_are_allowlisted() {
        let ok = TelemetryEvent::Tool {
            name: "get_today".into(),
            success: false,
            error_category: Some("network".into()),
        };
        assert_eq!(
            ok.to_json().unwrap(),
            json!({"kind": "tool", "name": "get_today", "outcome": "error", "error_category": "network"})
        );
        assert!(
            TelemetryEvent::Tool {
                name: "evil".into(),
                success: true,
                error_category: None
            }
            .to_json()
            .is_none()
        );
        assert!(
            TelemetryEvent::Prompt { name: "x".into() }
                .to_json()
                .is_none()
        );
        assert!(
            TelemetryEvent::Command {
                name: "rm".into(),
                success: true
            }
            .to_json()
            .is_none()
        );
        assert!(package_version().ends_with("-rust"));
    }
}
