//! WHOOP API HTTP client: bearer auth, JSON parsing, 429 retry with backoff,
//! one-shot 401 token refresh, and opt-in shared caching.

use super::{ApiError, BoxFuture, GetOptions, WHOOP_API_BASE_URL, WhoopApi};
use crate::cache::{DEFAULT_TTL_MS, MemoryCache};
use crate::js;
use crate::logging::Logger;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Maximum retries for 429 responses.
const MAX_RETRIES: u32 = 3;
/// Base backoff delay (1s, 2s, 4s).
const BASE_RETRY_DELAY_MS: u64 = 1000;
/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap for server-controlled `Retry-After`.
const MAX_RETRY_AFTER_MS: f64 = 60_000.0;

/// Callback that refreshes the access token after a 401.
pub type TokenRefresher = Arc<dyn Fn() -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

/// Options for [`WhoopClient::new`].
#[derive(Clone, Default)]
pub struct WhoopClientOptions {
    pub access_token: String,
    /// Override the base URL (tests).
    pub base_url: Option<String>,
    pub on_token_refresh: Option<TokenRefresher>,
    pub logger: Option<Logger>,
    pub request_id: Option<String>,
    pub cache: Option<Arc<MemoryCache>>,
}

/// Authenticated WHOOP API client.
pub struct WhoopClient {
    base_url: String,
    access_token: Mutex<String>,
    on_token_refresh: Option<TokenRefresher>,
    logger: Option<Logger>,
    cache: Option<Arc<MemoryCache>>,
    http: reqwest::Client,
}

struct Response {
    status: u16,
    status_text: String,
    retry_after: Option<String>,
    body: Result<bytes::Bytes, ()>,
}

impl Response {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Deterministic cache key: `GET:` + path with sorted query parameters.
pub fn cache_key(path: &str) -> String {
    let Some((base, query)) = path.split_once('?') else {
        return format!("GET:{path}");
    };
    let mut pairs = js::parse_query(query);
    js::sort_pairs(&mut pairs);
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", js::form_encode(k), js::form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        format!("GET:{base}")
    } else {
        format!("GET:{base}?{query}")
    }
}

/// Parse `Retry-After` seconds into milliseconds (capped), like `Number(header)`.
fn parse_retry_after(header: Option<&str>) -> Option<f64> {
    let raw = header?.trim();
    let seconds = if raw.is_empty() {
        0.0
    } else {
        raw.parse::<f64>().ok()?
    };
    if seconds.is_nan() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1000.0).min(MAX_RETRY_AFTER_MS))
}

fn parse_error_body(body: &Result<bytes::Bytes, ()>) -> Value {
    match body {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        }
        Err(()) => Value::Null,
    }
}

fn parse_json(body: Result<bytes::Bytes, ()>) -> Result<Value, ApiError> {
    let bytes =
        body.map_err(|()| ApiError::Other("Failed to read WHOOP API response body.".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::Other(format!("Invalid JSON from WHOOP API: {e}")))
}

impl WhoopClient {
    /// Create a client.
    pub fn new(options: WhoopClientOptions) -> Self {
        let logger = match (&options.logger, &options.request_id) {
            (Some(logger), Some(id)) => Some(logger.with_request_id(id)),
            (logger, _) => logger.clone(),
        };
        Self {
            base_url: options
                .base_url
                .unwrap_or_else(|| WHOOP_API_BASE_URL.to_string()),
            access_token: Mutex::new(options.access_token),
            on_token_refresh: options.on_token_refresh,
            logger,
            cache: options.cache,
            http: crate::net::shared_client(),
        }
    }

    fn token(&self) -> String {
        self.access_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn fetch(&self, url: &str, token: &str) -> Result<Response, ApiError> {
        let started = Instant::now();
        let request = self
            .http
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .timeout(REQUEST_TIMEOUT)
            .send();
        match request.await {
            Ok(response) => {
                let status = response.status();
                if let Some(logger) = &self.logger {
                    logger.debug(
                        "whoop api request",
                        &[
                            ("url", Value::from(url)),
                            ("status", Value::from(status.as_u16())),
                            (
                                "durationMs",
                                Value::from(started.elapsed().as_millis() as u64),
                            ),
                        ],
                    );
                }
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = response.bytes().await.map_err(|_| ());
                Ok(Response {
                    status: status.as_u16(),
                    status_text: status.canonical_reason().unwrap_or("").to_string(),
                    retry_after,
                    body,
                })
            }
            Err(error) => {
                if let Some(logger) = &self.logger {
                    let message = if error.is_timeout() {
                        "whoop api timeout"
                    } else {
                        "whoop api network error"
                    };
                    logger.error(
                        message,
                        &[
                            ("url", Value::from(url)),
                            (
                                "durationMs",
                                Value::from(started.elapsed().as_millis() as u64),
                            ),
                            ("error", Value::from(error.to_string())),
                        ],
                    );
                }
                Err(ApiError::Network(error.to_string()))
            }
        }
    }

    async fn do_get(&self, path: &str) -> Result<Value, ApiError> {
        let url = format!("{}{path}", self.base_url);
        let mut current_token = self.token();
        let mut last_error: Option<ApiError> = None;
        let mut last_retry_after: Option<Option<String>> = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0
                && let Some(retry_after) = &last_retry_after
            {
                let delay = parse_retry_after(retry_after.as_deref())
                    .unwrap_or((BASE_RETRY_DELAY_MS * 2u64.pow(attempt - 1)) as f64);
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            }

            let response = self.fetch(&url, &current_token).await?;
            if response.ok() {
                return parse_json(response.body);
            }
            let api_error = ApiError::Api {
                status: response.status,
                status_text: response.status_text.clone(),
                body: parse_error_body(&response.body),
            };

            if response.status == 429 {
                if let Some(logger) = &self.logger {
                    let mut extra = vec![
                        ("url", Value::from(url.as_str())),
                        ("attempt", Value::from(attempt)),
                    ];
                    if let Some(ms) = parse_retry_after(response.retry_after.as_deref()) {
                        extra.push(("retryAfterMs", js::num(ms)));
                    }
                    logger.warn("whoop api rate limited", &extra);
                }
                last_error = Some(api_error);
                last_retry_after = Some(response.retry_after);
                continue;
            }

            if response.status == 401
                && let Some(refresh) = &self.on_token_refresh
            {
                let new_token = refresh().await.map_err(ApiError::Auth)?;
                if let Some(logger) = &self.logger {
                    logger.info(
                        "whoop token refreshed",
                        &[("url", Value::from(url.as_str()))],
                    );
                }
                *self
                    .access_token
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = new_token.clone();
                current_token = new_token;
                let retry = self.fetch(&url, &current_token).await?;
                if retry.ok() {
                    return parse_json(retry.body);
                }
                return Err(ApiError::Api {
                    status: retry.status,
                    status_text: retry.status_text.clone(),
                    body: parse_error_body(&retry.body),
                });
            }

            return Err(api_error);
        }

        Err(last_error.unwrap_or_else(|| ApiError::Other("WHOOP API request failed.".into())))
    }
}

impl WhoopApi for WhoopClient {
    fn get<'a>(
        &'a self,
        path: &'a str,
        options: GetOptions,
    ) -> BoxFuture<'a, Result<Value, ApiError>> {
        Box::pin(async move {
            match (&self.cache, options.cache) {
                (Some(cache), true) => {
                    let ttl = options.ttl_ms.unwrap_or(DEFAULT_TTL_MS);
                    cache
                        .get_or_fetch(&cache_key(path), ttl, || self.do_get(path))
                        .await
                }
                _ => self.do_get(path).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_sort_query_parameters() {
        assert_eq!(cache_key("/v2/recovery"), "GET:/v2/recovery");
        assert_eq!(
            cache_key("/v2/cycle?limit=25&a=1"),
            "GET:/v2/cycle?a=1&limit=25"
        );
        assert_eq!(cache_key("/v2/cycle?"), "GET:/v2/cycle");
    }

    #[test]
    fn retry_after_matches_number_semantics() {
        assert_eq!(parse_retry_after(Some("2")), Some(2000.0));
        assert_eq!(parse_retry_after(Some("")), Some(0.0));
        assert_eq!(parse_retry_after(Some("600")), Some(60_000.0));
        assert_eq!(parse_retry_after(Some("-1")), None);
        assert_eq!(parse_retry_after(Some("soon")), None);
        assert_eq!(parse_retry_after(None), None);
    }
}
