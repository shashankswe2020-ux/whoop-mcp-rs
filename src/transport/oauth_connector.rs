//! OAuth 2.1 authorization server for remote MCP connectors (claude.ai web/mobile).
//!
//! One static client, PKCE S256, a connector password as the user credential,
//! one-time 60s authorization codes, and HS256 JWT access/refresh tokens with
//! refresh-token rotation and replay detection. Mirrors the TypeScript Express
//! app plus the MCP SDK's auth router.

use super::jwt::{
    ACCESS_TOKEN_TTL_SECONDS, REFRESH_TOKEN_TTL_SECONDS, SignOptions, TokenType, sign_token,
    verify_token,
};
use super::rate_limit::{RateLimiter, ip_key};
use crate::auth::callback_server::escape_html;
use crate::crypto::{base64url_encode, random_bytes, safe_token_compare, sha256};
use crate::http_util::{Body, full, response};
use crate::js;
use crate::logging::now_ms;
use crate::schema::{self, Issue};
use hyper::Response;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

/// Minimum connector password length.
pub const MIN_CONNECTOR_PASSWORD_LENGTH: usize = 12;
const AUTH_CODE_TTL_MS: f64 = 60_000.0;
const HKDF_SALT: &[u8] = b"whoop-mcp-jwt-v1";
const HKDF_INFO: &[u8] = b"jwt-signing";

const AUTHORIZE_PARAMS: [&str; 8] = [
    "client_id",
    "redirect_uri",
    "response_type",
    "scope",
    "state",
    "code_challenge",
    "code_challenge_method",
    "resource",
];

/// Derive the JWT signing key from the bearer token (HKDF-SHA256).
pub fn derive_jwt_secret(auth_token: &str) -> Vec<u8> {
    crate::crypto::hkdf_sha256(auth_token.as_bytes(), HKDF_SALT, HKDF_INFO, 32)
}

/// Parse `ALLOWED_REDIRECT_URIS` (comma-separated, trimmed, empties dropped).
pub fn parse_allowed_redirect_uris(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Static client configuration.
#[derive(Debug, Clone, Default)]
pub struct ConnectorClientConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
}

/// Connector configuration.
#[derive(Debug, Clone)]
pub struct OAuthConnectorOptions {
    pub connector_password: String,
    pub public_url: String,
    pub allowed_redirect_uris: Vec<String>,
    pub jwt_secret: Vec<u8>,
    pub scopes: Vec<String>,
    pub client: ConnectorClientConfig,
    /// Trust one proxy hop for client IPs (rate limiting).
    pub trust_proxy: bool,
}

#[derive(Debug, Clone)]
struct AuthCode {
    client_id: String,
    code_challenge: String,
    redirect_uri: String,
    scopes: Vec<String>,
    resource: Option<String>,
    expires_at: f64,
    consumed: bool,
}

/// Identity attached to an authenticated MCP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInfo {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<i64>,
}

/// Incoming request fields needed by the connector.
pub struct ConnectorRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    pub remote_ip: &'a str,
    pub forwarded_for: Option<&'a str>,
}

enum OAuthFailure {
    /// `OAuthError` subclass: status 400 (500 for server_error) with code/description.
    OAuth(&'static str, String),
    /// Plain error: reported as `server_error` / "Internal Server Error".
    Plain,
}

impl OAuthFailure {
    fn invalid_request(message: String) -> Self {
        Self::OAuth("invalid_request", message)
    }
}

/// The OAuth connector.
pub struct OAuthConnector {
    options: OAuthConnectorOptions,
    issuer: url::Url,
    client: Value,
    auth_codes: Mutex<HashMap<String, AuthCode>>,
    used_jtis: Mutex<HashMap<String, f64>>,
    authorize_limiter: RateLimiter,
    token_limiter: RateLimiter,
}

fn validate_public_url(public_url: &str) -> Result<url::Url, String> {
    let parsed = url::Url::parse(public_url)
        .map_err(|_| format!("PUBLIC_URL is not a valid URL: {public_url}"))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "PUBLIC_URL must use https:// (got {}:). OAuth requires HTTPS to prevent token interception.",
            parsed.scheme()
        ));
    }
    if parsed.fragment().is_some_and(|f| !f.is_empty()) {
        return Err(format!("Issuer URL must not have a fragment: {parsed}"));
    }
    if parsed.query().is_some_and(|q| !q.is_empty()) {
        return Err(format!("Issuer URL must not have a query string: {parsed}"));
    }
    Ok(parsed)
}

fn form_params(raw: &str) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in js::parse_query(raw) {
        match map.get_mut(&key) {
            Some(Value::Array(items)) => items.push(Value::from(value)),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, Value::from(value)]);
            }
            None => {
                map.insert(key, Value::from(value));
            }
        }
    }
    map
}

fn string_param<'a>(params: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn is_url(value: &str) -> bool {
    url::Url::parse(value).is_ok()
}

fn loopback(host: Option<&str>) -> bool {
    matches!(host, Some("localhost" | "127.0.0.1" | "[::1]"))
}

/// RFC 8252 §7.3 loopback port relaxation used by the SDK.
fn redirect_uri_matches(requested: &str, registered: &str) -> bool {
    if requested == registered {
        return true;
    }
    let (Ok(req), Ok(reg)) = (url::Url::parse(requested), url::Url::parse(registered)) else {
        return false;
    };
    if !loopback(req.host_str()) || !loopback(reg.host_str()) {
        return false;
    }
    req.scheme() == reg.scheme()
        && req.host_str() == reg.host_str()
        && req.path() == reg.path()
        && req.query() == reg.query()
}

fn custom_issue(path: &str, message: &str) -> Issue {
    Issue {
        fields: vec![("code", Value::from("custom"))],
        path: vec![schema::PathSeg::Key(path.into())],
        message: message.into(),
    }
}

fn url_issue(path: &str) -> Issue {
    Issue {
        fields: vec![
            ("code", Value::from("invalid_format")),
            ("format", Value::from("url")),
        ],
        path: vec![schema::PathSeg::Key(path.into())],
        message: "Invalid URL".into(),
    }
}

/// Validate `params` against a Zod-like object shape, plus URL checks.
fn check_params(
    params: &Map<String, Value>,
    shape: Value,
    required: &[&str],
    url_fields: &[&str],
    refine_redirect: bool,
) -> Result<(), Vec<Issue>> {
    let schema = json!({"type": "object", "properties": shape, "required": required});
    let mut issues = schema::validate(&schema, Some(&Value::Object(params.clone())))
        .err()
        .unwrap_or_default();
    for field in url_fields {
        if let Some(value) = string_param(params, field)
            && !is_url(value)
        {
            if refine_redirect {
                issues.push(custom_issue(field, "redirect_uri must be a valid URL"));
            } else {
                issues.push(url_issue(field));
            }
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

fn string() -> Value {
    json!({"type": "string"})
}

fn literal(value: &str) -> Value {
    json!({"type": "string", "enum": [value]})
}

fn express_json(status: u16, value: &Value, extra: &[(&str, &str)]) -> Response<Body> {
    let mut headers = vec![("Content-Type", "application/json; charset=utf-8")];
    headers.extend_from_slice(extra);
    response(status, &headers, full(js::stringify(value)))
}

fn oauth_error_json(code: &str, description: &str) -> Value {
    json!({"error": code, "error_description": description})
}

fn not_found(method: &str, path: &str) -> Response<Body> {
    let body = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot {method} {}</pre>\n</body>\n</html>\n",
        escape_html(path)
    );
    response(
        404,
        &[
            ("Content-Security-Policy", "default-src 'none'"),
            ("X-Content-Type-Options", "nosniff"),
            ("Content-Type", "text/html; charset=utf-8"),
        ],
        full(body),
    )
}

fn method_not_allowed(method: &str, allowed: &str, extra: &[(&str, &str)]) -> Response<Body> {
    let mut headers = vec![("Allow", allowed)];
    headers.extend_from_slice(extra);
    express_json(
        405,
        &oauth_error_json(
            "method_not_allowed",
            &format!("The method {method} is not allowed for this endpoint"),
        ),
        &headers,
    )
}

fn redirect(location: &str) -> Response<Body> {
    response(
        302,
        &[
            ("Location", location),
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Cache-Control", "no-store"),
        ],
        full(format!("Found. Redirecting to {location}")),
    )
}

fn with_headers(mut resp: Response<Body>, headers: &[(String, String)]) -> Response<Body> {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(value),
        ) {
            resp.headers_mut().insert(name, value);
        }
    }
    resp
}

fn render_password_page(params: &Map<String, Value>, error: Option<&str>) -> String {
    let hidden: Vec<String> = AUTHORIZE_PARAMS
        .iter()
        .map(|key| {
            string_param(params, key).map_or_else(String::new, |value| {
                format!(
                    "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                    escape_attr(key),
                    escape_attr(value)
                )
            })
        })
        .collect();
    let error_block = error.map_or_else(String::new, |e| {
        format!(
            "<p style=\"color:#c00;margin:0 0 12px 0;\">{}</p>",
            escape_attr(e)
        )
    });
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>WHOOP MCP — Authorize Connection</title>
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #f7f7f8; margin: 0; display: flex; align-items: center;
      justify-content: center; min-height: 100vh; padding: 20px; }}
    .card {{ background: #fff; border-radius: 12px; padding: 32px; max-width: 400px;
      width: 100%; box-shadow: 0 1px 3px rgba(0,0,0,.06), 0 8px 24px rgba(0,0,0,.04); }}
    h1 {{ font-size: 18px; margin: 0 0 8px 0; }}
    p {{ color: #555; font-size: 14px; line-height: 1.5; margin: 0 0 16px 0; }}
    label {{ display: block; font-size: 13px; font-weight: 500; margin-bottom: 6px; }}
    input[type=password] {{ width: 100%; padding: 10px 12px; border: 1px solid #ddd;
      border-radius: 6px; font-size: 14px; box-sizing: border-box; }}
    button {{ margin-top: 16px; width: 100%; padding: 10px; border: 0;
      border-radius: 6px; background: #111; color: #fff; font-size: 14px;
      font-weight: 500; cursor: pointer; }}
    button:hover {{ background: #333; }}
  </style>
</head>
<body>
  <div class="card">
    <h1>Authorize WHOOP MCP connection</h1>
    <p>Enter the connector password to grant this client access to your WHOOP data.</p>
    {error_block}
    <form method="POST" action="/authorize" autocomplete="off">
      {hidden}
      <label for="connector_password">Connector password</label>
      <input id="connector_password" name="connector_password" type="password"
             required autofocus>
      <button type="submit">Authorize</button>
    </form>
  </div>
</body>
</html>"#,
        hidden = hidden.join("\n      ")
    )
}

/// HTML escaping used by the password page (`'` → `&#39;`).
fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn authorize_page(status: u16, body: String) -> Response<Body> {
    response(
        status,
        &[
            ("Cache-Control", "no-store"),
            ("Content-Type", "text/html; charset=utf-8"),
            ("X-Frame-Options", "DENY"),
            (
                "Content-Security-Policy",
                "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
            ),
            ("Referrer-Policy", "no-referrer"),
            ("X-Content-Type-Options", "nosniff"),
        ],
        full(body),
    )
}

fn too_many_requests(headers: &[(String, String)]) -> Response<Body> {
    with_headers(
        response(
            429,
            &[("Content-Type", "text/html; charset=utf-8")],
            full("Too many requests, please try again later."),
        ),
        headers,
    )
}

impl OAuthConnector {
    /// Validate configuration and build the connector.
    pub fn new(options: OAuthConnectorOptions) -> Result<Self, String> {
        let length = js::js_len(&options.connector_password);
        if length < MIN_CONNECTOR_PASSWORD_LENGTH {
            return Err(format!(
                "MCP_CONNECTOR_PASSWORD must be at least {MIN_CONNECTOR_PASSWORD_LENGTH} characters. Provided length: {length}."
            ));
        }
        let issuer = validate_public_url(&options.public_url)?;
        let mut client = Map::new();
        client.insert(
            "client_id".into(),
            Value::from(options.client.client_id.clone()),
        );
        client.insert(
            "redirect_uris".into(),
            Value::from(options.client.redirect_uris.clone()),
        );
        if let Some(secret) = &options.client.client_secret {
            client.insert("client_secret".into(), Value::from(secret.clone()));
        }
        if let Some(name) = &options.client.client_name {
            client.insert("client_name".into(), Value::from(name.clone()));
        }
        Ok(Self {
            options,
            issuer,
            client: Value::Object(client),
            auth_codes: Mutex::new(HashMap::new()),
            used_jtis: Mutex::new(HashMap::new()),
            authorize_limiter: RateLimiter::new(60_000, 3),
            token_limiter: RateLimiter::new(60_000, 10),
        })
    }

    fn client_redirect_uris(&self) -> &[String] {
        &self.options.client.redirect_uris
    }

    /// Authorization server metadata (RFC 8414).
    pub fn metadata(&self) -> Value {
        let endpoint = |path: &str| {
            self.issuer
                .join(path)
                .map(|u| u.to_string())
                .unwrap_or_default()
        };
        json!({
            "issuer": self.issuer.as_str(),
            "authorization_endpoint": endpoint("/authorize"),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint": endpoint("/token"),
            "token_endpoint_auth_methods_supported": ["client_secret_post", "none"],
            "grant_types_supported": ["authorization_code", "refresh_token"]
        })
    }

    /// Protected resource metadata (RFC 9728) and the path it is served at.
    pub fn protected_resource(&self) -> (String, Value) {
        let path = self.issuer.path();
        let route = if path == "/" {
            "/.well-known/oauth-protected-resource".to_string()
        } else {
            format!("/.well-known/oauth-protected-resource{path}")
        };
        (
            route,
            json!({"resource": self.issuer.as_str(), "authorization_servers": [self.issuer.as_str()]}),
        )
    }

    fn client_ip(&self, request: &ConnectorRequest<'_>) -> String {
        let forwarded = request
            .forwarded_for
            .filter(|_| self.options.trust_proxy)
            .and_then(|xff| xff.split(',').map(str::trim).rfind(|s| !s.is_empty()));
        ip_key(forwarded.unwrap_or(request.remote_ip))
    }

    /// Route an OAuth request (`/authorize`, `/token`, `/register`, `/.well-known/*`).
    pub fn handle(&self, request: &ConnectorRequest<'_>) -> Response<Body> {
        let method = request.method;
        let path = request.path;
        let trimmed = if path.len() > 1 {
            path.trim_end_matches('/')
        } else {
            path
        };
        let (prm_route, prm) = self.protected_resource();
        match trimmed {
            "/authorize" => self.handle_authorize(request),
            "/token" => {
                if method != "POST" {
                    return method_not_allowed(
                        method,
                        "POST",
                        &[("Access-Control-Allow-Origin", "*")],
                    );
                }
                let rate = self.token_limiter.hit(&self.client_ip(request));
                if rate.limited {
                    return too_many_requests(&rate.headers);
                }
                let resp = self.handle_token(request);
                with_headers(resp, &rate.headers)
            }
            "/.well-known/oauth-authorization-server" => {
                self.metadata_response(method, &self.metadata())
            }
            route if route == prm_route => self.metadata_response(method, &prm),
            _ => not_found(method, path),
        }
    }

    fn metadata_response(&self, method: &str, metadata: &Value) -> Response<Body> {
        let cors = [("Access-Control-Allow-Origin", "*")];
        match method {
            "GET" | "HEAD" => express_json(200, metadata, &cors),
            _ => method_not_allowed(method, "GET, OPTIONS", &cors),
        }
    }

    fn body_params(&self, request: &ConnectorRequest<'_>) -> Option<Map<String, Value>> {
        let content_type = request.content_type?;
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        (essence == "application/x-www-form-urlencoded")
            .then(|| form_params(&String::from_utf8_lossy(request.body)))
    }

    fn handle_authorize(&self, request: &ConnectorRequest<'_>) -> Response<Body> {
        if request.method != "GET" && request.method != "POST" {
            return method_not_allowed(request.method, "GET, POST", &[]);
        }
        let rate = self.authorize_limiter.hit(&self.client_ip(request));
        if rate.limited {
            return too_many_requests(&rate.headers);
        }
        let resp = if request.method == "GET" {
            let params = form_params(request.query.unwrap_or(""));
            authorize_page(200, render_password_page(&params, None))
        } else {
            match self.body_params(request) {
                None => response(
                    500,
                    &[("Content-Type", "text/plain; charset=utf-8")],
                    full("Internal Server Error"),
                ),
                Some(mut params) => {
                    let provided = string_param(&params, "connector_password").unwrap_or("");
                    if !safe_token_compare(provided, &self.options.connector_password) {
                        authorize_page(
                            401,
                            render_password_page(&params, Some("Incorrect password. Try again.")),
                        )
                    } else {
                        params.remove("connector_password");
                        self.authorize(&params)
                    }
                }
            }
        };
        with_headers(resp, &rate.headers)
    }

    /// SDK authorization handler after password verification.
    fn authorize(&self, params: &Map<String, Value>) -> Response<Body> {
        let direct = |failure: OAuthFailure| -> Response<Body> {
            let (status, code, description) = match failure {
                OAuthFailure::OAuth(code, description) => (400, code, description),
                OAuthFailure::Plain => (500, "server_error", "Internal Server Error".into()),
            };
            express_json(
                status,
                &oauth_error_json(code, &description),
                &[("Cache-Control", "no-store")],
            )
        };
        if let Err(issues) = check_params(
            params,
            json!({"client_id": string(), "redirect_uri": string()}),
            &["client_id"],
            &["redirect_uri"],
            true,
        ) {
            return direct(OAuthFailure::invalid_request(schema::issues_json(&issues)));
        }
        let client_id = string_param(params, "client_id").unwrap_or("");
        if client_id != self.options.client.client_id {
            return direct(OAuthFailure::OAuth(
                "invalid_client",
                "Invalid client_id".into(),
            ));
        }
        let registered = self.client_redirect_uris();
        let redirect_uri = match string_param(params, "redirect_uri") {
            Some(requested) => {
                if !registered
                    .iter()
                    .any(|r| redirect_uri_matches(requested, r))
                {
                    return direct(OAuthFailure::invalid_request(
                        "Unregistered redirect_uri".into(),
                    ));
                }
                requested.to_string()
            }
            None if registered.len() == 1 => registered[0].clone(),
            None => {
                return direct(OAuthFailure::invalid_request(
                    "redirect_uri must be specified when client has multiple registered URIs"
                        .into(),
                ));
            }
        };

        let error_redirect =
            |code: &str, description: &str, state: Option<&str>| -> Response<Body> {
                let Ok(mut url) = url::Url::parse(&redirect_uri) else {
                    return direct(OAuthFailure::Plain);
                };
                set_query_param(&mut url, "error", code);
                set_query_param(&mut url, "error_description", description);
                if let Some(state) = state.filter(|s| !s.is_empty()) {
                    set_query_param(&mut url, "state", state);
                }
                redirect(url.as_str())
            };
        let shape = json!({
            "response_type": literal("code"),
            "code_challenge": string(),
            "code_challenge_method": literal("S256"),
            "scope": string(),
            "state": string(),
            "resource": string()
        });
        if let Err(issues) = check_params(
            params,
            shape,
            &["response_type", "code_challenge", "code_challenge_method"],
            &["resource"],
            false,
        ) {
            return error_redirect("invalid_request", &schema::issues_json(&issues), None);
        }
        let state = string_param(params, "state");
        let code_challenge = string_param(params, "code_challenge").unwrap_or("");
        let scopes: Vec<String> = string_param(params, "scope")
            .map(|s| s.split(' ').map(str::to_string).collect())
            .unwrap_or_default();
        let resource = string_param(params, "resource")
            .and_then(|r| url::Url::parse(r).ok())
            .map(|u| u.to_string());

        if code_challenge.is_empty() || !self.options.allowed_redirect_uris.contains(&redirect_uri)
        {
            return error_redirect("server_error", "Internal Server Error", state);
        }
        let code = base64url_encode(&random_bytes(32));
        {
            let mut codes = self
                .auth_codes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = now_ms();
            codes.retain(|_, record| record.expires_at >= now);
            codes.insert(
                code.clone(),
                AuthCode {
                    client_id: client_id.to_string(),
                    code_challenge: code_challenge.to_string(),
                    redirect_uri: redirect_uri.clone(),
                    scopes,
                    resource,
                    expires_at: now + AUTH_CODE_TTL_MS,
                    consumed: false,
                },
            );
        }
        let Ok(mut url) = url::Url::parse(&redirect_uri) else {
            return error_redirect("server_error", "Internal Server Error", state);
        };
        set_query_param(&mut url, "code", &code);
        if let Some(state) = state.filter(|s| !s.is_empty()) {
            set_query_param(&mut url, "state", state);
        }
        redirect(url.as_str())
    }

    fn handle_token(&self, request: &ConnectorRequest<'_>) -> Response<Body> {
        let cors = ("Access-Control-Allow-Origin", "*");
        let reply = |status: u16, value: Value, no_store: bool| {
            let mut headers = vec![cors];
            if no_store {
                headers.push(("Cache-Control", "no-store"));
            }
            express_json(status, &value, &headers)
        };
        let fail = |failure: OAuthFailure, no_store: bool| match failure {
            OAuthFailure::OAuth(code, description) => {
                reply(400, oauth_error_json(code, &description), no_store)
            }
            OAuthFailure::Plain => reply(
                500,
                oauth_error_json("server_error", "Internal Server Error"),
                no_store,
            ),
        };

        let params = self.body_params(request);
        let client_schema = json!({"type": "object", "properties": {"client_id": string(), "client_secret": string()}, "required": ["client_id"]});
        let body_value = params.clone().map(Value::Object);
        if let Err(issues) = schema::validate(&client_schema, body_value.as_ref()) {
            return fail(
                OAuthFailure::invalid_request(schema::issues_json(&issues)),
                false,
            );
        }
        let params = params.unwrap_or_default();
        if string_param(&params, "client_id") != Some(self.options.client.client_id.as_str()) {
            return fail(
                OAuthFailure::OAuth("invalid_client", "Invalid client_id".into()),
                false,
            );
        }
        if let Some(secret) = &self.options.client.client_secret {
            match string_param(&params, "client_secret") {
                None | Some("") => {
                    return fail(
                        OAuthFailure::OAuth("invalid_client", "Client secret is required".into()),
                        false,
                    );
                }
                Some(provided) if provided != secret => {
                    return fail(
                        OAuthFailure::OAuth("invalid_client", "Invalid client_secret".into()),
                        false,
                    );
                }
                _ => {}
            }
        }

        if let Err(issues) = check_params(
            &params,
            json!({"grant_type": string()}),
            &["grant_type"],
            &[],
            false,
        ) {
            return fail(
                OAuthFailure::invalid_request(schema::issues_json(&issues)),
                true,
            );
        }
        let result = match string_param(&params, "grant_type").unwrap_or("") {
            "authorization_code" => self.exchange_code(&params),
            "refresh_token" => self.exchange_refresh(&params),
            _ => Err(OAuthFailure::OAuth(
                "unsupported_grant_type",
                "The grant type is not supported by this authorization server.".into(),
            )),
        };
        match result {
            Ok(tokens) => reply(200, tokens, true),
            Err(failure) => fail(failure, true),
        }
    }

    fn exchange_code(&self, params: &Map<String, Value>) -> Result<Value, OAuthFailure> {
        let shape = json!({"code": string(), "code_verifier": string(), "redirect_uri": string(), "resource": string()});
        check_params(
            params,
            shape,
            &["code", "code_verifier"],
            &["resource"],
            false,
        )
        .map_err(|issues| OAuthFailure::invalid_request(schema::issues_json(&issues)))?;
        let code = string_param(params, "code").unwrap_or("");
        let verifier = string_param(params, "code_verifier").unwrap_or("");
        let now = now_ms();
        let mut codes = self
            .auth_codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = codes
            .get_mut(code)
            .filter(|r| r.expires_at >= now && !r.consumed)
            .ok_or(OAuthFailure::Plain)?;
        if base64url_encode(&sha256(verifier.as_bytes())) != record.code_challenge {
            return Err(OAuthFailure::OAuth(
                "invalid_grant",
                "code_verifier does not match the challenge".into(),
            ));
        }
        record.consumed = true;
        let record = record.clone();
        drop(codes);
        if let Some(redirect_uri) = string_param(params, "redirect_uri")
            && redirect_uri != record.redirect_uri
        {
            return Err(OAuthFailure::Plain);
        }
        if !self
            .options
            .allowed_redirect_uris
            .contains(&record.redirect_uri)
            || record.client_id != self.options.client.client_id
        {
            return Err(OAuthFailure::Plain);
        }
        Ok(self.issue_tokens(&record.scopes, record.resource))
    }

    fn exchange_refresh(&self, params: &Map<String, Value>) -> Result<Value, OAuthFailure> {
        let shape = json!({"refresh_token": string(), "scope": string(), "resource": string()});
        check_params(params, shape, &["refresh_token"], &["resource"], false)
            .map_err(|issues| OAuthFailure::invalid_request(schema::issues_json(&issues)))?;
        let refresh_token = string_param(params, "refresh_token").unwrap_or("");
        let verified = verify_token(refresh_token, &self.options.jwt_secret)
            .map_err(|_| OAuthFailure::Plain)?;
        if verified.token_type != TokenType::Refresh
            || verified.client_id != self.options.client.client_id
        {
            return Err(OAuthFailure::Plain);
        }
        let jti = verified.jti.clone().ok_or(OAuthFailure::Plain)?;
        {
            let mut used = self
                .used_jtis
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = now_ms();
            used.retain(|_, expires| *expires > now);
            if used.contains_key(&jti) {
                return Err(OAuthFailure::Plain);
            }
            used.insert(jti, verified.expires_at as f64 * 1000.0);
        }
        let requested: Option<Vec<String>> =
            string_param(params, "scope").map(|s| s.split(' ').map(str::to_string).collect());
        let scopes = match requested {
            Some(scopes) if !scopes.is_empty() => {
                if scopes.iter().any(|s| !verified.scopes.contains(s)) {
                    return Err(OAuthFailure::Plain);
                }
                scopes
            }
            _ => verified.scopes.clone(),
        };
        if let Some(resource) = string_param(params, "resource") {
            let normalized = url::Url::parse(resource)
                .map(|u| u.to_string())
                .unwrap_or_default();
            if Some(normalized) != verified.resource {
                return Err(OAuthFailure::Plain);
            }
        }
        Ok(self.issue_tokens(&scopes, verified.resource))
    }

    fn issue_tokens(&self, scopes: &[String], resource: Option<String>) -> Value {
        let base = SignOptions {
            client_id: self.options.client.client_id.clone(),
            scopes: scopes.to_vec(),
            resource,
            ttl_seconds: ACCESS_TOKEN_TTL_SECONDS,
            token_type: TokenType::Access,
            jti: None,
        };
        let access = sign_token(&base, &self.options.jwt_secret);
        let refresh = sign_token(
            &SignOptions {
                ttl_seconds: REFRESH_TOKEN_TTL_SECONDS,
                token_type: TokenType::Refresh,
                jti: Some(base64url_encode(&random_bytes(16))),
                ..base
            },
            &self.options.jwt_secret,
        );
        json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TOKEN_TTL_SECONDS,
            "refresh_token": refresh,
            "scope": scopes.join(" ")
        })
    }

    /// Verify an OAuth access token presented on `/mcp`.
    pub fn verify_access_token(&self, token: &str) -> Result<AuthInfo, String> {
        let verified = verify_token(token, &self.options.jwt_secret)?;
        if verified.token_type != TokenType::Access {
            return Err("Token is not an access token".into());
        }
        Ok(AuthInfo {
            client_id: verified.client_id,
            scopes: verified.scopes,
            expires_at: Some(verified.expires_at),
        })
    }

    /// The registered client (for diagnostics).
    pub fn client(&self) -> &Value {
        &self.client
    }
}

/// `URLSearchParams.set`: replace all values of `key`, appending when absent.
fn set_query_param(url: &mut url::Url, key: &str, value: &str) {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    match pairs.iter().position(|(k, _)| k == key) {
        Some(index) => {
            pairs[index].1 = value.to_string();
            let mut seen = false;
            pairs.retain(|(k, _)| {
                if k != key {
                    return true;
                }
                let keep = !seen;
                seen = true;
                keep
            });
        }
        None => pairs.push((key.to_string(), value.to_string())),
    }
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", js::form_encode(k), js::form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    url.set_query(if query.is_empty() { None } else { Some(&query) });
}
