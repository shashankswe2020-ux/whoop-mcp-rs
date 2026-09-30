//! Direct WHOOP endpoint tools: profile, body measurement, collections, and by-id lookups.

use super::ToolResult;
use super::dates::{CollectionParams, build_collection_query};
use crate::api::{ENDPOINT_BODY_MEASUREMENT, ENDPOINT_USER_PROFILE, GetOptions, WhoopApi};
use serde_json::Value;

/// `get_profile`
pub async fn get_profile(client: &dyn WhoopApi) -> ToolResult<Value> {
    Ok(client
        .get(ENDPOINT_USER_PROFILE, GetOptions::default())
        .await?)
}

/// `get_body_measurement`
pub async fn get_body_measurement(client: &dyn WhoopApi) -> ToolResult<Value> {
    Ok(client
        .get(ENDPOINT_BODY_MEASUREMENT, GetOptions::default())
        .await?)
}

/// `get_{recovery,sleep,workout,cycle}_collection`
pub async fn get_collection(
    client: &dyn WhoopApi,
    endpoint: &str,
    params: &CollectionParams,
    now: f64,
) -> ToolResult<Value> {
    let query = build_collection_query(params, now)?;
    Ok(client
        .get(&format!("{endpoint}{query}"), GetOptions::default())
        .await?)
}

/// `get_{sleep,workout,cycle}_by_id`
pub async fn get_by_id(client: &dyn WhoopApi, endpoint: &str, id: &str) -> ToolResult<Value> {
    Ok(client
        .get(&format!("{endpoint}/{id}"), GetOptions::default())
        .await?)
}
