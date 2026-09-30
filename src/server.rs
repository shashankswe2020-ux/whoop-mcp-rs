//! MCP protocol handling: JSON-RPC dispatch, tool execution with input/output
//! contracts, resources, prompts, and telemetry hooks.

use crate::api::{
    ApiError, ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, ENDPOINT_WORKOUT, WhoopApi,
};
use crate::catalog::{self, PrivacyMode};
use crate::js;
use crate::schema::{self, format_issues, issues_json};
use crate::telemetry::{Telemetry, TelemetryEvent};
use crate::tools::dates::CollectionParams;
use crate::tools::{
    ToolError, ToolResult, baselines, basic, calendar, compare, sleep_debt, today, trend, weekly,
};
use crate::{prompts, resources};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Newest protocol version supported.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// All supported protocol versions.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

/// Source of the current time (injectable for tests).
pub trait Clock: Send + Sync {
    /// Current epoch milliseconds.
    fn now_ms(&self) -> f64;
}

/// Wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> f64 {
        crate::logging::now_ms()
    }
}

/// Server configuration.
#[derive(Clone)]
pub struct ServerOptions {
    pub privacy_mode: PrivacyMode,
    /// Skip resource registration (`WHOOP_MCP_DISABLE_RESOURCES=1`).
    pub disable_resources: bool,
    pub telemetry: Option<Arc<Telemetry>>,
    pub clock: Arc<dyn Clock>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            privacy_mode: PrivacyMode::Standard,
            disable_resources: false,
            telemetry: None,
            clock: Arc::new(SystemClock),
        }
    }
}

/// Kind of an incoming JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageKind {
    Request { id: Value, method: String },
    Notification { method: String },
    Response,
    Invalid,
}

/// Classify a JSON-RPC 2.0 message.
pub fn classify(message: &Value) -> MessageKind {
    let Some(obj) = message.as_object() else {
        return MessageKind::Invalid;
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return MessageKind::Invalid;
    }
    if obj.get("params").is_some_and(|p| !p.is_object()) {
        return MessageKind::Invalid;
    }
    let valid_id = |id: &Value| id.is_string() || id.as_f64().is_some_and(|n| n.fract() == 0.0);
    match (obj.get("method").and_then(Value::as_str), obj.get("id")) {
        (Some(method), Some(id)) if valid_id(id) => MessageKind::Request {
            id: id.clone(),
            method: method.into(),
        },
        (Some(_), Some(_)) => MessageKind::Invalid,
        (Some(method), None) => MessageKind::Notification {
            method: method.into(),
        },
        (None, Some(_)) if obj.contains_key("result") || obj.contains_key("error") => {
            MessageKind::Response
        }
        _ => MessageKind::Invalid,
    }
}

/// Whether a message is an `initialize` request.
pub fn is_initialize_request(message: &Value) -> bool {
    matches!(classify(message), MessageKind::Request { ref method, .. } if method == "initialize")
}

fn success(id: &Value, result: Value) -> Value {
    json!({"result": result, "jsonrpc": "2.0", "id": id})
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Tool error result (key order follows the SDK's `CallToolResultSchema` output).
fn tool_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn handler_error(message: &str) -> Value {
    tool_error(message)
}

/// Error message shown to the model for a failed tool.
fn error_message(error: &ToolError) -> &'static str {
    match error {
        ToolError::Api(ApiError::Api { .. }) => "",
        ToolError::Api(ApiError::Auth(_)) => {
            "WHOOP authentication failed. Run setup --verify to reconnect."
        }
        ToolError::Api(ApiError::Network(_)) => {
            "Network error: Unable to reach the WHOOP API. Check your internet connection."
        }
        ToolError::Invalid(_) => {
            "Invalid input or data. Check the requested parameters and date range."
        }
        ToolError::Api(ApiError::Other(_)) | ToolError::Other(_) => {
            "An unexpected error occurred. Check configuration and retry."
        }
    }
}

/// Telemetry error category for a failed tool.
pub fn classify_tool_error(error: &ToolError) -> &'static str {
    match error {
        ToolError::Api(ApiError::Auth(_)) => "api_auth",
        ToolError::Api(ApiError::Api { status, .. }) => match status {
            401 | 403 => "api_auth",
            429 => "api_rate_limit",
            s if *s >= 500 => "api_server",
            _ => "api_client",
        },
        ToolError::Api(ApiError::Network(_)) => "network",
        ToolError::Invalid(_) => "invalid_data",
        ToolError::Api(ApiError::Other(_)) | ToolError::Other(_) => "unexpected",
    }
}

fn json_content(data: Value) -> Value {
    let mut data = data;
    js::normalize(&mut data);
    json!({"content": [{"type": "text", "text": js::stringify_pretty(&data)}], "structuredContent": data})
}

fn slice10(value: &mut Value, key: &str) {
    if let Some(Value::String(text)) = value.get(key) {
        let short: String = text.chars().take(10).collect();
        value[key] = Value::from(short);
    }
}

/// Reduce timestamps to dates in aggregate privacy mode.
pub fn project_aggregate_dates(mut data: Value) -> Value {
    for key in ["period", "period_a", "period_b"] {
        if let Some(period) = data.get_mut(key).filter(|p| js::truthy(Some(p))) {
            slice10(period, "start");
            slice10(period, "end");
        }
    }
    slice10(&mut data, "week_start");
    slice10(&mut data, "week_end");
    if let Some(requested) = data.pointer_mut("/data_quality/requested_period") {
        slice10(requested, "start");
        slice10(requested, "end");
    }
    data
}

fn param_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn param_num(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn request_schema(method: &str) -> Option<Value> {
    let record = |values: Value| json!({"type": "object", "propertyNames": {"type": "string"}, "additionalProperties": values});
    let params = match method {
        "initialize" => json!({
            "type": "object",
            "properties": {
                "protocolVersion": {"type": "string"},
                "capabilities": {"type": "object", "additionalProperties": {}},
                "clientInfo": {"type": "object", "properties": {"name": {"type": "string"}, "version": {"type": "string"}}, "required": ["name", "version"], "additionalProperties": {}}
            },
            "required": ["protocolVersion", "capabilities", "clientInfo"],
            "additionalProperties": {}
        }),
        "tools/call" => json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "arguments": record(json!({}))},
            "required": ["name"],
            "additionalProperties": {}
        }),
        "prompts/get" => json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "arguments": record(json!({"type": "string"}))},
            "required": ["name"],
            "additionalProperties": {}
        }),
        "resources/read" => json!({
            "type": "object",
            "properties": {"uri": {"type": "string"}},
            "required": ["uri"],
            "additionalProperties": {}
        }),
        _ => return None,
    };
    Some(params)
}

/// The MCP server.
pub struct McpServer {
    client: Arc<dyn WhoopApi>,
    options: ServerOptions,
    /// In-flight requests keyed by scope and id; the flag marks cancellation.
    inflight: Mutex<HashMap<String, bool>>,
}

impl McpServer {
    /// Create a server backed by `client`.
    pub fn new(client: Arc<dyn WhoopApi>, options: ServerOptions) -> Self {
        Self {
            client,
            options,
            inflight: Mutex::new(HashMap::new()),
        }
    }

    fn standard(&self) -> bool {
        self.options.privacy_mode == PrivacyMode::Standard
    }

    fn resources_enabled(&self) -> bool {
        self.standard() && !self.options.disable_resources
    }

    fn record(&self, event: TelemetryEvent) {
        if let Some(telemetry) = &self.options.telemetry {
            telemetry.record(&event);
        }
    }

    /// Handle one JSON-RPC message; returns the response for requests.
    pub async fn handle(&self, message: &Value) -> Option<Value> {
        self.handle_scoped("", message).await
    }

    /// Handle a message within a session scope (cancellations only affect that scope).
    pub async fn handle_scoped(&self, scope: &str, message: &Value) -> Option<Value> {
        match classify(message) {
            MessageKind::Request { id, method } => {
                let key = format!("{scope}:{id}");
                self.inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key.clone(), false);
                let params = message.get("params");
                let response = self.dispatch(&id, &method, params).await;
                let cancelled = self
                    .inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key)
                    .unwrap_or(false);
                (!cancelled).then_some(response)
            }
            MessageKind::Notification { method } => {
                if method == "notifications/cancelled"
                    && let Some(id) = message.pointer("/params/requestId")
                    && let Some(flag) = self
                        .inflight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&format!("{scope}:{id}"))
                {
                    *flag = true;
                }
                None
            }
            MessageKind::Response | MessageKind::Invalid => None,
        }
    }

    async fn dispatch(&self, id: &Value, method: &str, params: Option<&Value>) -> Value {
        let registered = match method {
            "initialize" | "ping" | "tools/list" | "tools/call" => true,
            "resources/list" | "resources/templates/list" | "resources/read" => {
                self.resources_enabled()
            }
            "prompts/list" | "prompts/get" => self.standard(),
            _ => false,
        };
        if !registered {
            return failure(id, -32601, "Method not found");
        }
        if let Some(schema) = request_schema(method)
            && let Err(issues) = schema::validate_at(&schema, params, &["params"])
        {
            return failure(id, -32603, &issues_json(&issues));
        }
        let params = params.cloned().unwrap_or(Value::Object(Map::new()));
        match method {
            "initialize" => success(id, self.initialize(&params)),
            "ping" => success(id, json!({})),
            "tools/list" => success(
                id,
                json!({"tools": catalog::tools(self.options.privacy_mode)}),
            ),
            "tools/call" => success(id, self.call_tool(&params).await),
            "resources/list" => success(id, resources::list()),
            "resources/templates/list" => success(id, json!({"resourceTemplates": []})),
            "resources/read" => {
                let raw = js::s(params.get("uri"));
                let Ok(uri) = url::Url::parse(raw) else {
                    return failure(id, -32603, "Invalid URL");
                };
                match resources::read(uri.as_str(), self.client.as_ref()).await {
                    Some(result) => success(id, result),
                    None => failure(
                        id,
                        -32602,
                        &format!("MCP error -32602: Resource {uri} not found"),
                    ),
                }
            }
            "prompts/list" => success(id, prompts::list()),
            "prompts/get" => {
                let name = js::s(params.get("name"));
                match prompts::get(name, params.get("arguments")) {
                    Some(result) => {
                        self.record(TelemetryEvent::Prompt { name: name.into() });
                        success(id, result)
                    }
                    None => failure(
                        id,
                        -32602,
                        &format!("MCP error -32602: Prompt {name} not found"),
                    ),
                }
            }
            _ => failure(id, -32601, "Method not found"),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = js::s(params.get("protocolVersion"));
        let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        let mut capabilities = Map::new();
        capabilities.insert("tools".into(), json!({"listChanged": true}));
        if self.resources_enabled() {
            capabilities.insert("resources".into(), json!({"listChanged": true}));
        }
        if self.standard() {
            capabilities.insert("prompts".into(), json!({"listChanged": true}));
        }
        json!({
            "protocolVersion": version,
            "capabilities": capabilities,
            "serverInfo": {"name": "whoop-mcp", "version": env!("CARGO_PKG_VERSION")}
        })
    }

    async fn call_tool(&self, params: &Value) -> Value {
        let name = js::s(params.get("name")).to_string();
        let mode = self.options.privacy_mode;
        let Some(definition) = catalog::tool(mode, &name) else {
            return tool_error(&format!("MCP error -32602: Tool {name} not found"));
        };
        let input = match schema::validate(&definition["inputSchema"], params.get("arguments")) {
            Ok(input) => input,
            Err(issues) => {
                return tool_error(&format!(
                    "MCP error -32602: Input validation error: Invalid arguments for tool {name}: {}",
                    format_issues(&issues)
                ));
            }
        };

        let (response, outcome) = match self.run_tool(&name, &input).await {
            Err(error) => {
                let message = match &error {
                    ToolError::Api(ApiError::Api { status, .. }) => {
                        format!("WHOOP API returned {status}. Retry later or verify authorization.")
                    }
                    other => error_message(other).to_string(),
                };
                (handler_error(&message), Some(classify_tool_error(&error)))
            }
            Ok(mut data) => {
                js::normalize(&mut data);
                match schema::validate(&definition["outputSchema"], Some(&data)) {
                    Ok(validated) => {
                        let projected = if mode == PrivacyMode::Aggregate {
                            project_aggregate_dates(validated)
                        } else {
                            validated
                        };
                        (json_content(projected), None)
                    }
                    Err(_) => (
                        handler_error("WHOOP data did not match the expected output contract."),
                        Some("output_contract"),
                    ),
                }
            }
        };
        if self.standard() {
            self.record(TelemetryEvent::Tool {
                name,
                success: outcome.is_none(),
                error_category: outcome.map(str::to_string),
            });
        }
        response
    }

    async fn run_tool(&self, name: &str, args: &Value) -> ToolResult<Value> {
        let client = self.client.as_ref();
        let now = self.options.clock.now_ms();
        let collection = |endpoint: &'static str| {
            let params = CollectionParams {
                start: param_str(args, "start"),
                end: param_str(args, "end"),
                limit: param_num(args, "limit"),
                next_token: param_str(args, "nextToken"),
            };
            (endpoint, params)
        };
        let id = || match args.get("id") {
            Some(Value::Number(n)) => js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
            other => js::s(other).to_string(),
        };
        match name {
            "get_profile" => basic::get_profile(client).await,
            "get_body_measurement" => basic::get_body_measurement(client).await,
            "get_recovery_collection"
            | "get_sleep_collection"
            | "get_workout_collection"
            | "get_cycle_collection" => {
                let endpoint = match name {
                    "get_recovery_collection" => ENDPOINT_RECOVERY,
                    "get_sleep_collection" => ENDPOINT_SLEEP,
                    "get_workout_collection" => ENDPOINT_WORKOUT,
                    _ => ENDPOINT_CYCLE,
                };
                let (endpoint, params) = collection(endpoint);
                basic::get_collection(client, endpoint, &params, now).await
            }
            "get_sleep_by_id" => basic::get_by_id(client, ENDPOINT_SLEEP, &id()).await,
            "get_workout_by_id" => basic::get_by_id(client, ENDPOINT_WORKOUT, &id()).await,
            "get_cycle_by_id" => basic::get_by_id(client, ENDPOINT_CYCLE, &id()).await,
            "get_weekly_summary" => {
                weekly::get_weekly_summary(client, param_str(args, "week_start").as_deref(), now)
                    .await
            }
            "compare_periods" => {
                let params = compare::ComparePeriodsParams {
                    period_a_start: param_str(args, "period_a_start").unwrap_or_default(),
                    period_a_end: param_str(args, "period_a_end").unwrap_or_default(),
                    period_b_start: param_str(args, "period_b_start").unwrap_or_default(),
                    period_b_end: param_str(args, "period_b_end").unwrap_or_default(),
                };
                compare::compare_periods(client, &params).await
            }
            "get_trend" => {
                let metric = param_str(args, "metric").unwrap_or_default();
                trend::get_trend(client, &metric, param_num(args, "days"), now).await
            }
            "get_today" => today::get_today(client, now).await,
            "get_calendar" => {
                calendar::get_calendar(
                    client,
                    param_num(args, "days"),
                    param_str(args, "start").as_deref(),
                    now,
                )
                .await
            }
            "get_baselines" => {
                baselines::get_baselines(client, param_num(args, "baseline_days"), now).await
            }
            "get_sleep_debt" => {
                sleep_debt::get_sleep_debt(
                    client,
                    param_num(args, "days"),
                    param_str(args, "start").as_deref(),
                    now,
                )
                .await
            }
            other => Err(ToolError::Other(format!("Unknown tool {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert_eq!(
            classify(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})),
            MessageKind::Request {
                id: json!(1),
                method: "ping".into()
            }
        );
        assert_eq!(
            classify(&json!({"jsonrpc": "2.0", "method": "x"})),
            MessageKind::Notification { method: "x".into() }
        );
        assert_eq!(
            classify(&json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            MessageKind::Response
        );
        assert_eq!(
            classify(&json!({"jsonrpc": "2.0", "id": 999, "foo": 1})),
            MessageKind::Invalid
        );
        assert_eq!(
            classify(&json!({"jsonrpc": "2.0", "id": 1.5, "method": "x"})),
            MessageKind::Invalid
        );
        assert_eq!(classify(&json!([1])), MessageKind::Invalid);
    }

    #[test]
    fn projects_aggregate_dates() {
        let data = json!({
            "period": {"start": "2024-01-01T00:00:00.000Z", "end": "2024-01-31T00:00:00.000Z", "days": 30},
            "week_start": "2024-01-01T00:00:00.000Z",
            "data_quality": {"evaluated_at": "x", "requested_period": {"start": "2024-01-01T10:00:00.000Z", "end": "2024-01-02T10:00:00.000Z"}}
        });
        let projected = project_aggregate_dates(data);
        assert_eq!(
            projected["period"],
            json!({"start": "2024-01-01", "end": "2024-01-31", "days": 30})
        );
        assert_eq!(projected["week_start"], "2024-01-01");
        assert_eq!(
            projected["data_quality"]["requested_period"]["end"],
            "2024-01-02"
        );
    }
}
