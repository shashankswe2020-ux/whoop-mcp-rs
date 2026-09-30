//! WHOOP REST API: endpoints, typed errors, client, and pagination.

pub mod client;
pub mod pagination;

use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Boxed future used by the object-safe [`WhoopApi`] trait.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// WHOOP API v2 base URL.
pub const WHOOP_API_BASE_URL: &str = "https://api.prod.whoop.com/developer";
/// OAuth authorization endpoint.
pub const WHOOP_AUTH_URL: &str = "https://api.prod.whoop.com/oauth/oauth2/auth";
/// OAuth token endpoint.
pub const WHOOP_TOKEN_URL: &str = "https://api.prod.whoop.com/oauth/oauth2/token";
/// OAuth scopes required by the tools.
pub const WHOOP_REQUIRED_SCOPES: &str =
    "offline read:recovery read:cycles read:workout read:sleep read:profile read:body_measurement";
/// Default local OAuth redirect URI.
pub const DEFAULT_REDIRECT_URI: &str = "http://localhost:3000/callback";

pub const ENDPOINT_USER_PROFILE: &str = "/v2/user/profile/basic";
pub const ENDPOINT_BODY_MEASUREMENT: &str = "/v2/user/measurement/body";
pub const ENDPOINT_RECOVERY: &str = "/v2/recovery";
pub const ENDPOINT_SLEEP: &str = "/v2/activity/sleep";
pub const ENDPOINT_WORKOUT: &str = "/v2/activity/workout";
pub const ENDPOINT_CYCLE: &str = "/v2/cycle";

/// Configured redirect URI (`WHOOP_REDIRECT_URI`, falling back to the default).
pub fn redirect_uri() -> String {
    std::env::var("WHOOP_REDIRECT_URI")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string())
}

/// Errors raised while talking to WHOOP.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// Non-2xx response.
    Api {
        status: u16,
        status_text: String,
        body: Value,
    },
    /// Transport-level failure (DNS, TCP, TLS, timeout).
    Network(String),
    /// Token refresh failed during automatic 401 recovery.
    Auth(String),
    /// Any other failure (authentication flow, malformed payloads).
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api {
                status,
                status_text,
                ..
            } => write!(f, "WHOOP API error: {status} {status_text}"),
            Self::Network(_) => f.write_str(
                "Network error: Unable to reach the WHOOP API. Check your internet connection.",
            ),
            Self::Auth(_) => f.write_str(
                "Authentication error: Failed to refresh token. Re-authentication may be required.",
            ),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ApiError {}

/// Per-request options for [`WhoopApi::get`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GetOptions {
    /// Serve from / store in the shared cache when one is configured.
    pub cache: bool,
    /// TTL override in milliseconds.
    pub ttl_ms: Option<u64>,
}

impl GetOptions {
    /// Cached request with the given TTL.
    pub fn cached(ttl_ms: u64) -> Self {
        Self {
            cache: true,
            ttl_ms: Some(ttl_ms),
        }
    }
}

/// Read-only access to the WHOOP API.
pub trait WhoopApi: Send + Sync {
    /// GET `path` (relative to the API base URL) and return the parsed JSON body.
    fn get<'a>(
        &'a self,
        path: &'a str,
        options: GetOptions,
    ) -> BoxFuture<'a, Result<Value, ApiError>>;
}
