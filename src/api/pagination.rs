//! Auto-pagination for WHOOP collection endpoints with rate-limit safety caps.

use super::{ApiError, GetOptions, WhoopApi};
use crate::js;
use serde_json::Value;
use std::time::Duration;

/// Absolute maximum records per pagination run.
pub const ABSOLUTE_MAX_RECORDS: usize = 500;

/// Pagination limits.
#[derive(Debug, Clone, Copy)]
pub struct PageOptions {
    pub max_records: usize,
    pub max_pages: usize,
    pub inter_page_delay_ms: u64,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            max_records: 100,
            max_pages: 20,
            inter_page_delay_ms: 200,
        }
    }
}

/// Collected records and whether pagination stopped early.
#[derive(Debug, Clone, PartialEq)]
pub struct Pages {
    pub records: Vec<Value>,
    pub truncated: bool,
}

/// Pagination failure.
#[derive(Debug, Clone, PartialEq)]
pub enum PageError {
    /// The WHOOP request failed.
    Api(ApiError),
    /// A page did not contain a `records` array.
    Shape,
    /// A page failed schema validation (analytics sources only).
    Invalid,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(error) => error.fmt(f),
            Self::Shape => f.write_str("Cannot read properties of undefined (reading 'length')"),
            Self::Invalid => f.write_str("Invalid WHOOP collection page."),
        }
    }
}

impl From<ApiError> for PageError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

/// JS `String(token)` for a truthy `next_token`, or `None` when falsy.
fn next_token(page: &Value) -> Option<String> {
    let token = page.get("next_token");
    if !js::truthy(token) {
        return None;
    }
    Some(match token? {
        Value::String(s) => s.clone(),
        Value::Number(n) => js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(b) => b.to_string(),
        Value::Array(_) => String::new(),
        _ => "[object Object]".into(),
    })
}

fn validate_page(page: &Value) -> Result<(), PageError> {
    let records_ok = page.get("records").is_some_and(Value::is_array);
    let token_ok = match page.get("next_token") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => js::js_len(s) <= 4096,
        Some(_) => false,
    };
    if records_ok && token_ok {
        Ok(())
    } else {
        Err(PageError::Invalid)
    }
}

/// Follow `next_token` across pages until exhausted or a cap is reached.
///
/// With `validate`, each page must match `{ records: unknown[], next_token?: string<=4096 | null }`.
pub async fn fetch_all_pages(
    client: &dyn WhoopApi,
    path: &str,
    options: PageOptions,
    validate: bool,
) -> Result<Pages, PageError> {
    let max_records = options.max_records.min(ABSOLUTE_MAX_RECORDS);
    let mut records: Vec<Value> = Vec::new();
    let mut current = path.to_string();
    let mut pages = 0;

    while pages < options.max_pages {
        if pages > 0 && options.inter_page_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(options.inter_page_delay_ms)).await;
        }
        let page = client.get(&current, GetOptions::default()).await?;
        if validate {
            validate_page(&page)?;
        }
        pages += 1;
        let batch = page
            .get("records")
            .and_then(Value::as_array)
            .ok_or(PageError::Shape)?;
        let remaining = max_records.saturating_sub(records.len());
        if batch.len() > remaining {
            records.extend(batch.iter().take(remaining).cloned());
            return Ok(Pages {
                records,
                truncated: true,
            });
        }
        records.extend(batch.iter().cloned());
        let token = next_token(&page);
        if records.len() >= max_records {
            let truncated = !matches!(page.get("next_token"), None | Some(Value::Null));
            return Ok(Pages { records, truncated });
        }
        let Some(token) = token else {
            return Ok(Pages {
                records,
                truncated: false,
            });
        };
        let separator = if path.contains('?') { '&' } else { '?' };
        current = format!(
            "{path}{separator}nextToken={}",
            js::encode_uri_component(&token)
        );
    }
    Ok(Pages {
        records,
        truncated: true,
    })
}
