//! MCP resources: ambient health context backed by the shared API cache.

use crate::api::{GetOptions, WhoopApi};
use crate::tools::today::{CYCLE_TTL_MS, DYNAMIC_TTL_MS, PROFILE_TTL_MS};
use serde_json::{Value, json};

/// A registered resource.
pub struct ResourceDefinition {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    path: &'static str,
    ttl_ms: u64,
    latest_record: Option<&'static str>,
}

/// The four resources, in registration order.
pub const RESOURCES: [ResourceDefinition; 4] = [
    ResourceDefinition {
        uri: "whoop://v2/user/recovery/latest",
        name: "Latest Recovery",
        description: "Most recent recovery score including HRV, resting heart rate, and SpO2.",
        path: "/v2/recovery?limit=1",
        ttl_ms: DYNAMIC_TTL_MS,
        latest_record: Some("No recovery data available."),
    },
    ResourceDefinition {
        uri: "whoop://v2/user/sleep/latest",
        name: "Latest Sleep",
        description: "Most recent sleep record including stages, duration, and performance.",
        path: "/v2/activity/sleep?limit=1",
        ttl_ms: DYNAMIC_TTL_MS,
        latest_record: Some("No sleep data available."),
    },
    ResourceDefinition {
        uri: "whoop://v2/user/cycle/latest",
        name: "Latest Cycle",
        description: "Most recent physiological cycle including strain and calorie data.",
        path: "/v2/cycle?limit=1",
        ttl_ms: CYCLE_TTL_MS,
        latest_record: Some("No cycle data available."),
    },
    ResourceDefinition {
        uri: "whoop://v2/user/profile",
        name: "User Profile",
        description: "Authenticated user's basic profile — name and email.",
        path: "/v2/user/profile/basic",
        ttl_ms: PROFILE_TTL_MS,
        latest_record: None,
    },
];

/// `resources/list` payload.
pub fn list() -> Value {
    let resources: Vec<Value> = RESOURCES
        .iter()
        .map(|r| json!({"uri": r.uri, "name": r.name, "description": r.description, "mimeType": "application/json"}))
        .collect();
    json!({ "resources": resources })
}

async fn fetch(definition: &ResourceDefinition, client: &dyn WhoopApi) -> Option<Value> {
    let data = client
        .get(definition.path, GetOptions::cached(definition.ttl_ms))
        .await
        .ok()?;
    let Some(empty_message) = definition.latest_record else {
        return Some(data);
    };
    let records = data.get("records")?.as_array()?;
    Some(
        records
            .first()
            .cloned()
            .unwrap_or_else(|| json!({ "message": empty_message })),
    )
}

/// Read a resource by normalized URI (`None` when the URI is not registered).
pub async fn read(uri: &str, client: &dyn WhoopApi) -> Option<Value> {
    let definition = RESOURCES.iter().find(|r| r.uri == uri)?;
    let content = match fetch(definition, client).await {
        Some(data) => {
            json!({"uri": uri, "mimeType": "application/json", "text": crate::js::stringify_pretty(&data)})
        }
        None => {
            eprintln!("[whoop-mcp] Resource read failed for {}", definition.uri);
            let error =
                json!({"error": "Resource unavailable. Retry later or verify authorization."});
            json!({"uri": uri, "mimeType": "application/json", "text": crate::js::stringify(&error)})
        }
    };
    Some(json!({ "contents": [content] }))
}
