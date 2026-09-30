//! OAuth 2.0 Authorization Code flow (with PKCE) for the WHOOP API.

use super::callback_server::{CallbackServerOptions, start_callback_server};
use super::oauth_lock::{LockOptions, acquire_oauth_flow_lock};
use super::token_store::{OAuthTokens, is_token_expired, load_tokens, save_tokens};
use crate::api::{WHOOP_AUTH_URL, WHOOP_REQUIRED_SCOPES, WHOOP_TOKEN_URL, redirect_uri};
use crate::crypto::{base64url_encode, hex, random_bytes, sha256};
use crate::logging::now_ms;
use serde_json::Value;
use std::path::PathBuf;

/// OAuth client configuration.
#[derive(Debug, Clone, Default)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Override the redirect URI (defaults to `WHOOP_REDIRECT_URI`).
    pub redirect_uri: Option<String>,
    /// Token directory (defaults to `~/.whoop-mcp`).
    pub token_dir: Option<PathBuf>,
    /// Callback port; must match the redirect URI when provided.
    pub port: Option<u16>,
    /// Override the token endpoint (tests).
    pub token_url: Option<String>,
}

impl OAuthConfig {
    fn redirect(&self) -> String {
        self.redirect_uri.clone().unwrap_or_else(redirect_uri)
    }

    fn token_endpoint(&self) -> &str {
        self.token_url.as_deref().unwrap_or(WHOOP_TOKEN_URL)
    }
}

/// OAuth failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    /// Transport-level failure reaching the token endpoint.
    Network(String),
    /// Any other failure, with a user-facing message.
    Other(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(_) => f.write_str(
                "Network error: Unable to reach the WHOOP API. Check your internet connection.",
            ),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for OAuthError {}

/// Raw token endpoint response.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: f64,
    pub token_type: String,
    pub scope: String,
}

impl TokenResponse {
    fn from_json(value: &Value) -> Self {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Self {
            access_token: text("access_token"),
            refresh_token: text("refresh_token"),
            expires_in: value
                .get("expires_in")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN),
            token_type: text("token_type"),
            scope: text("scope"),
        }
    }
}

struct LocalRedirect {
    host: String,
    port: u16,
    callback_path: String,
}

fn is_loopback_redirect_prefix(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };
    let Some(rest) = ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .find_map(|h| rest.strip_prefix(h))
    else {
        return false;
    };
    let rest = match rest.strip_prefix(':') {
        Some(port) => {
            let digits = port.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return false;
            }
            &port[digits..]
        }
        None => rest,
    };
    rest.is_empty() || rest.starts_with('/')
}

fn parse_local_redirect(config: &OAuthConfig) -> Result<LocalRedirect, OAuthError> {
    let value = config.redirect();
    let err = |m: &str| OAuthError::Other(m.to_string());
    let url = url::Url::parse(&value).map_err(|_| err("WHOOP_REDIRECT_URI must be a valid URL"))?;
    if url.scheme() != "http" {
        return Err(err("WHOOP_REDIRECT_URI must use HTTP"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(err("WHOOP_REDIRECT_URI must not include credentials"));
    }
    if value.contains('?') || value.contains('#') {
        return Err(err(
            "WHOOP_REDIRECT_URI must not include a query string or fragment",
        ));
    }
    if !is_loopback_redirect_prefix(&value) {
        return Err(err(
            "WHOOP_REDIRECT_URI must use localhost, 127.0.0.1, or [::1]",
        ));
    }
    let host = match url.host_str() {
        Some("localhost" | "127.0.0.1") => "127.0.0.1",
        Some("[::1]") => "::1",
        _ => {
            return Err(err(
                "WHOOP_REDIRECT_URI must use localhost, 127.0.0.1, or [::1]",
            ));
        }
    };
    let port = url.port().unwrap_or(80);
    if port < 1 {
        return Err(err(
            "WHOOP_REDIRECT_URI must use a port between 1 and 65535",
        ));
    }
    if config.port.is_some_and(|p| p != port) {
        return Err(err("OAuth callback port must match WHOOP_REDIRECT_URI"));
    }
    Ok(LocalRedirect {
        host: host.into(),
        port,
        callback_path: url.path().to_string(),
    })
}

/// Build the WHOOP authorization URL.
pub fn build_authorization_url(
    config: &OAuthConfig,
    state: &str,
    code_challenge: Option<&str>,
) -> String {
    let mut url = url::Url::parse(WHOOP_AUTH_URL).expect("static URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", &config.redirect())
            .append_pair("scope", WHOOP_REQUIRED_SCOPES)
            .append_pair("state", state);
        if let Some(challenge) = code_challenge.filter(|c| !c.is_empty()) {
            query
                .append_pair("code_challenge", challenge)
                .append_pair("code_challenge_method", "S256");
        }
    }
    url.to_string()
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    crate::js::search_params(pairs)
}

async fn post_token(config: &OAuthConfig, body: String) -> Result<(u16, Value), reqwest::Error> {
    let response = crate::net::shared_client()
        .post(config.token_endpoint())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, value))
}

fn error_description(value: &Value) -> &str {
    value
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code_for_tokens(
    code: &str,
    config: &OAuthConfig,
    code_verifier: Option<&str>,
) -> Result<TokenResponse, OAuthError> {
    let redirect = config.redirect();
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
        ("redirect_uri", redirect.as_str()),
    ];
    if let Some(verifier) = code_verifier.filter(|v| !v.is_empty()) {
        pairs.push(("code_verifier", verifier));
    }
    let (status, value) = post_token(config, form_body(&pairs))
        .await
        .map_err(|_| OAuthError::Other("fetch failed".into()))?;
    if !(200..300).contains(&status) {
        return Err(OAuthError::Other(format!(
            "Token exchange failed ({status}): {}",
            error_description(&value)
        )));
    }
    Ok(TokenResponse::from_json(&value))
}

/// Use a refresh token to obtain a new access token.
pub async fn refresh_access_token(
    refresh_token: &str,
    config: &OAuthConfig,
) -> Result<TokenResponse, OAuthError> {
    let pairs = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
        ("scope", "offline"),
    ];
    let (status, value) = post_token(config, form_body(&pairs))
        .await
        .map_err(|e| OAuthError::Network(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(OAuthError::Other(format!(
            "Token refresh failed ({status}): {}",
            error_description(&value)
        )));
    }
    Ok(TokenResponse::from_json(&value))
}

/// Convert a token response into stored tokens, preserving the prior refresh token when omitted.
pub fn to_oauth_tokens(
    response: &TokenResponse,
    existing_refresh_token: Option<&str>,
) -> OAuthTokens {
    let refresh_token = if response.refresh_token.is_empty() {
        existing_refresh_token.unwrap_or("").to_string()
    } else {
        response.refresh_token.clone()
    };
    OAuthTokens {
        access_token: response.access_token.clone(),
        refresh_token,
        expires_at: now_ms() + response.expires_in * 1000.0,
        token_type: response.token_type.clone(),
    }
}

/// Open a URL in the default browser (http/https only, no shell interpolation).
pub fn open_browser(url: &str) -> Result<(), OAuthError> {
    let parsed = url::Url::parse(url).map_err(|_| OAuthError::Other("Invalid URL".into()))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(OAuthError::Other(format!(
            "Refusing to open browser for non-HTTP(S) URL scheme: {}:",
            parsed.scheme()
        )));
    }
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/c", "start", "\"\"", url])
    } else {
        ("xdg-open", vec![url])
    };
    let spawned = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(_) => {
            eprintln!(
                "\nCould not open browser automatically. Please open this URL manually:\n{url}\n"
            );
        }
    }
    Ok(())
}

/// Return a valid access token: cached, refreshed, or via the full browser flow.
pub async fn authenticate(config: &OAuthConfig) -> Result<String, OAuthError> {
    if config.client_id.is_empty() {
        return Err(OAuthError::Other(
            "Missing WHOOP_CLIENT_ID. Set it in your environment variables.".into(),
        ));
    }
    if config.client_secret.is_empty() {
        return Err(OAuthError::Other(
            "Missing WHOOP_CLIENT_SECRET. Set it in your environment variables.".into(),
        ));
    }
    let token_dir = config.token_dir.as_deref();
    if let Some(existing) = load_tokens(token_dir) {
        if !is_token_expired(&existing) {
            eprintln!("Using cached WHOOP tokens (not expired).");
            return Ok(existing.access_token);
        }
        eprintln!("Cached tokens expired, attempting refresh...");
        match refresh_access_token(&existing.refresh_token, config).await {
            Ok(refreshed) => {
                let tokens = to_oauth_tokens(&refreshed, Some(&existing.refresh_token));
                save_tokens(&tokens, token_dir).map_err(|e| OAuthError::Other(e.to_string()))?;
                eprintln!("Token refresh successful.");
                return Ok(tokens.access_token);
            }
            Err(error @ OAuthError::Network(_)) => return Err(error),
            Err(error) => eprintln!("Token refresh failed, starting full OAuth flow: {error}"),
        }
    } else {
        eprintln!("No cached tokens found, starting OAuth flow...");
    }
    perform_oauth_flow_with_lock(config).await
}

async fn perform_oauth_flow_with_lock(config: &OAuthConfig) -> Result<String, OAuthError> {
    let token_dir = config.token_dir.as_deref();
    let lock = acquire_oauth_flow_lock(token_dir, LockOptions::default())
        .await
        .map_err(|e| OAuthError::Other(e.to_string()))?;
    let result = match load_tokens(token_dir) {
        Some(tokens) if !is_token_expired(&tokens) => {
            eprintln!("Using WHOOP tokens created by another process.");
            Ok(tokens.access_token)
        }
        _ => perform_oauth_flow(config).await,
    };
    let released = lock.release();
    let token = result?;
    released.map_err(|e| OAuthError::Other(e.to_string()))?;
    Ok(token)
}

async fn perform_oauth_flow(config: &OAuthConfig) -> Result<String, OAuthError> {
    let state = hex(&random_bytes(16));
    let verifier = base64url_encode(&random_bytes(32));
    let challenge = base64url_encode(&sha256(verifier.as_bytes()));
    let redirect = parse_local_redirect(config)?;

    let handle = start_callback_server(CallbackServerOptions {
        host: redirect.host,
        port: redirect.port,
        callback_path: redirect.callback_path,
        ..CallbackServerOptions::new(state.clone())
    })
    .await;

    let auth_url = build_authorization_url(config, &state, Some(&challenge));
    open_browser(&auth_url)?;
    eprintln!(
        "\nWaiting for WHOOP authorization...\nIf the browser didn't open, visit:\n{auth_url}\n"
    );

    let callback = handle
        .result
        .await
        .map_err(|e| OAuthError::Other(e.to_string()))?
        .map_err(OAuthError::Other)?;
    let response = exchange_code_for_tokens(&callback.code, config, Some(&verifier)).await?;
    let tokens = to_oauth_tokens(&response, None);
    save_tokens(&tokens, config.token_dir.as_deref())
        .map_err(|e| OAuthError::Other(e.to_string()))?;
    Ok(tokens.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(redirect: &str) -> OAuthConfig {
        OAuthConfig {
            client_id: "id".into(),
            client_secret: "secret".into(),
            redirect_uri: Some(redirect.into()),
            ..OAuthConfig::default()
        }
    }

    #[test]
    fn validates_loopback_redirects() {
        let ok = parse_local_redirect(&config("http://localhost:3000/callback"))
            .ok()
            .unwrap();
        assert_eq!(
            (ok.host.as_str(), ok.port, ok.callback_path.as_str()),
            ("127.0.0.1", 3000, "/callback")
        );
        let v6 = parse_local_redirect(&config("http://[::1]:8080/cb"))
            .ok()
            .unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 8080));
        let error = |uri: &str| match parse_local_redirect(&config(uri)) {
            Err(OAuthError::Other(message)) => message,
            _ => String::new(),
        };
        assert_eq!(error("not a url"), "WHOOP_REDIRECT_URI must be a valid URL");
        assert_eq!(
            error("https://localhost/cb"),
            "WHOOP_REDIRECT_URI must use HTTP"
        );
        assert_eq!(
            error("http://u:p@localhost/cb"),
            "WHOOP_REDIRECT_URI must not include credentials"
        );
        assert_eq!(
            error("http://localhost/cb?x=1"),
            "WHOOP_REDIRECT_URI must not include a query string or fragment"
        );
        assert_eq!(
            error("http://example.com/cb"),
            "WHOOP_REDIRECT_URI must use localhost, 127.0.0.1, or [::1]"
        );
        assert_eq!(
            error("http://2130706433/cb"),
            "WHOOP_REDIRECT_URI must use localhost, 127.0.0.1, or [::1]"
        );
        assert_eq!(
            error("http://localhost:0/cb"),
            "WHOOP_REDIRECT_URI must use a port between 1 and 65535"
        );
        let mut mismatch = config("http://localhost:3000/cb");
        mismatch.port = Some(4000);
        assert!(parse_local_redirect(&mismatch).is_err());
    }

    #[test]
    fn builds_authorization_url() {
        let url =
            build_authorization_url(&config("http://localhost:3000/callback"), "st", Some("ch"));
        assert_eq!(
            url,
            "https://api.prod.whoop.com/oauth/oauth2/auth?response_type=code&client_id=id&redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fcallback&scope=offline+read%3Arecovery+read%3Acycles+read%3Aworkout+read%3Asleep+read%3Aprofile+read%3Abody_measurement&state=st&code_challenge=ch&code_challenge_method=S256"
        );
    }

    #[test]
    fn preserves_refresh_token_when_omitted() {
        let response = TokenResponse {
            access_token: "a".into(),
            refresh_token: String::new(),
            expires_in: 3600.0,
            token_type: "bearer".into(),
            scope: String::new(),
        };
        let tokens = to_oauth_tokens(&response, Some("old"));
        assert_eq!(tokens.refresh_token, "old");
        assert!(tokens.expires_at > now_ms() + 3_500_000.0);
    }

    #[test]
    fn refuses_non_http_browser_urls() {
        assert!(open_browser("file:///etc/passwd").is_err());
        assert!(open_browser("javascript:alert(1)").is_err());
    }
}
