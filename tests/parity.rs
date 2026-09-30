//! Differential parity test against the TypeScript implementation.
//!
//! `tests/fixtures/parity.json` is produced by `scripts/generate-parity-fixtures.mjs`,
//! which runs the TypeScript `whoop-ai-mcp` server against a synthetic WHOOP
//! dataset with a frozen clock. This test replays every case through the Rust
//! server with the same dataset, fake API, and clock, and requires identical
//! MCP results (including the pretty-printed text content).

use serde_json::{Map, Value, json};
use std::sync::Arc;
use whoop_mcp::api::{ApiError, BoxFuture, GetOptions, WhoopApi};
use whoop_mcp::catalog::PrivacyMode;
use whoop_mcp::js;
use whoop_mcp::server::{Clock, McpServer, ServerOptions};

struct FixedClock(f64);

impl Clock for FixedClock {
    fn now_ms(&self) -> f64 {
        self.0
    }
}

/// Mirror of the fake client in `scripts/generate-parity-fixtures.mjs`.
struct FakeWhoop {
    profile: Value,
    body: Value,
    collections: Map<String, Value>,
    errors: Map<String, Value>,
}

fn parse_int(raw: &str) -> f64 {
    let digits: String = raw
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(f64::NAN)
}

impl FakeWhoop {
    fn respond(&self, path: &str) -> Result<Value, ApiError> {
        let not_found = || ApiError::Api {
            status: 404,
            status_text: "Error".into(),
            body: json!({}),
        };
        let (base, query) = path.split_once('?').unwrap_or((path, ""));
        let params = js::parse_query(query);
        let param = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        for (prefix, status) in &self.errors {
            if base == prefix || base.starts_with(&format!("{prefix}/")) {
                let status = status.as_u64().unwrap_or(500) as u16;
                return Err(ApiError::Api {
                    status,
                    status_text: "Error".into(),
                    body: json!({}),
                });
            }
        }
        match base {
            "/v2/user/profile/basic" => return Ok(self.profile.clone()),
            "/v2/user/measurement/body" => return Ok(self.body.clone()),
            _ => {}
        }
        for (endpoint, records) in &self.collections {
            let records = records.as_array().expect("records array");
            if let Some(id) = base.strip_prefix(&format!("{endpoint}/")) {
                return records
                    .iter()
                    .find(|r| match &r["id"] {
                        Value::String(s) => s == id,
                        Value::Number(n) => {
                            js::number_to_string(n.as_f64().unwrap_or(f64::NAN)) == id
                        }
                        _ => false,
                    })
                    .cloned()
                    .ok_or_else(not_found);
            }
            if base != endpoint {
                continue;
            }
            let key = if endpoint == "/v2/recovery" {
                "created_at"
            } else {
                "start"
            };
            let time = |r: &Value| js::parse(r[key].as_str().unwrap_or(""));
            let start = param("start").map(|s| js::parse(&s));
            let end = param("end").map(|s| js::parse(&s));
            let mut filtered: Vec<&Value> = records
                .iter()
                .filter(|r| start.is_none_or(|s| time(r) >= s) && end.is_none_or(|e| time(r) < e))
                .collect();
            filtered.sort_by(|a, b| {
                time(b)
                    .partial_cmp(&time(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let limit =
                parse_int(&param("limit").unwrap_or_else(|| "10".into())).min(25.0) as usize;
            let offset = parse_int(&param("nextToken").unwrap_or_else(|| "0".into())) as usize;
            let page: Vec<Value> = filtered
                .iter()
                .skip(offset)
                .take(limit)
                .map(|r| (*r).clone())
                .collect();
            let next = if offset + limit < filtered.len() {
                Value::from((offset + limit).to_string())
            } else {
                Value::Null
            };
            return Ok(json!({"records": page, "next_token": next}));
        }
        Err(not_found())
    }
}

impl WhoopApi for FakeWhoop {
    fn get<'a>(
        &'a self,
        path: &'a str,
        _options: GetOptions,
    ) -> BoxFuture<'a, Result<Value, ApiError>> {
        Box::pin(async move { self.respond(path) })
    }
}

fn load_fixture() -> Value {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/parity.json"
    ))
    .expect("parity fixture present");
    serde_json::from_str(&raw).expect("valid fixture JSON")
}

fn dataset(fixture: &Value, variant: &str) -> FakeWhoop {
    let base = &fixture["base"];
    let variant = &fixture["variants"][variant];
    let collections = if variant["empty"].as_bool().unwrap_or(false) {
        base["collections"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| (k.clone(), json!([])))
            .collect()
    } else {
        base["collections"].as_object().unwrap().clone()
    };
    FakeWhoop {
        profile: base["profile"].clone(),
        body: base["body"].clone(),
        collections,
        errors: variant["errors"].as_object().cloned().unwrap_or_default(),
    }
}

#[tokio::test(start_paused = true)]
async fn tool_and_resource_results_match_typescript() {
    let fixture = load_fixture();
    let now = js::parse(fixture["now"].as_str().unwrap());
    let cases = fixture["cases"].as_array().unwrap();
    assert!(
        cases.len() >= 60,
        "fixture should contain the full scenario set"
    );
    let mut failures = Vec::new();

    for (index, case) in cases.iter().enumerate() {
        let variant = case["dataset"].as_str().unwrap();
        let privacy = PrivacyMode::parse(case["privacy"].as_str().unwrap()).unwrap();
        let server = McpServer::new(
            Arc::new(dataset(&fixture, variant)),
            ServerOptions {
                privacy_mode: privacy,
                clock: Arc::new(FixedClock(now)),
                ..ServerOptions::default()
            },
        );
        let request = if case["kind"] == "tool" {
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": case["name"], "arguments": case["args"]}})
        } else {
            json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": case["uri"]}})
        };
        let response = server.handle(&request).await.expect("response");
        let actual = response
            .get("result")
            .or_else(|| response.get("error"))
            .cloned()
            .unwrap_or(Value::Null);
        let expected = &case["result"];
        if js::stringify(&actual) != js::stringify(expected) {
            let label = format!(
                "#{index} {} {} [{variant}/{}]",
                case["kind"].as_str().unwrap(),
                case.get("name")
                    .or_else(|| case.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                case["privacy"].as_str().unwrap()
            );
            failures.push(format!(
                "{label}\n  expected: {}\n  actual:   {}",
                js::stringify(expected),
                js::stringify(&actual)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} parity mismatches:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
