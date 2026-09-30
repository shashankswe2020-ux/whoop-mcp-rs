//! Process entry: environment parsing, deferred WHOOP authentication,
//! transport wiring, and CLI dispatch.

use crate::api::client::{TokenRefresher, WhoopClient, WhoopClientOptions};
use crate::api::{ApiError, BoxFuture, GetOptions, WhoopApi};
use crate::auth::oauth::{
    OAuthConfig, OAuthError, authenticate, refresh_access_token, to_oauth_tokens,
};
use crate::auth::token_store::{load_tokens, save_tokens};
use crate::cache::MemoryCache;
use crate::catalog::PrivacyMode;
use crate::logging::{LogLevel, Logger};
use crate::server::{McpServer, ServerOptions};
use crate::telemetry::{Telemetry, TelemetryEvent};
use crate::transport::http::{HttpServerHandle, HttpServerOptions, create_http_server};
use crate::transport::oauth_connector::{
    ConnectorClientConfig, OAuthConnector, OAuthConnectorOptions, derive_jwt_secret,
    parse_allowed_redirect_uris,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn required_env(name: &str) -> Result<String, String> {
    env(name).filter(|v| !v.is_empty()).ok_or_else(|| {
        format!(
            "Missing required environment variable: {name}.\nSet it in your Claude Desktop config or shell environment.\nSee: https://github.com/shashankswe2020-ux/whoop-mcp#configuration"
        )
    })
}

fn describe(raw: Option<&str>) -> String {
    raw.map_or_else(|| "undefined".into(), str::to_string)
}

/// Transport selection (`MCP_TRANSPORT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    Stdio,
    Http,
    Both,
}

fn parse_transport() -> Result<TransportMode, String> {
    let raw = env("MCP_TRANSPORT");
    match raw
        .as_deref()
        .unwrap_or("stdio")
        .trim()
        .to_lowercase()
        .as_str()
    {
        "stdio" => Ok(TransportMode::Stdio),
        "http" => Ok(TransportMode::Http),
        "both" => Ok(TransportMode::Both),
        _ => Err(format!(
            "Invalid MCP_TRANSPORT: \"{}\". Must be one of: stdio, http, both.",
            describe(raw.as_deref())
        )),
    }
}

/// `Number.parseInt(raw, 10)`.
fn parse_int(raw: &str) -> Option<i64> {
    let trimmed = raw.trim_start();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let count = digits.bytes().take_while(u8::is_ascii_digit).count();
    digits[..count].parse::<i64>().ok().map(|n| n * sign)
}

fn parse_port() -> Result<u16, String> {
    let raw = env("MCP_PORT").unwrap_or_else(|| "3000".into());
    parse_int(&raw)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("Invalid MCP_PORT: \"{raw}\". Must be an integer 0-65535."))
}

fn parse_log_level() -> Result<LogLevel, String> {
    let raw = env("LOG_LEVEL");
    LogLevel::parse(
        raw.as_deref()
            .unwrap_or("info")
            .trim()
            .to_lowercase()
            .as_str(),
    )
    .ok_or_else(|| {
        format!(
            "Invalid LOG_LEVEL: \"{}\". Must be one of: debug, info, warn, error.",
            describe(raw.as_deref())
        )
    })
}

fn parse_log_format() -> Result<bool, String> {
    let raw = env("LOG_FORMAT");
    match raw
        .as_deref()
        .unwrap_or("json")
        .trim()
        .to_lowercase()
        .as_str()
    {
        "json" => Ok(false),
        "pretty" => Ok(true),
        _ => Err(format!(
            "Invalid LOG_FORMAT: \"{}\". Must be one of: json, pretty.",
            describe(raw.as_deref())
        )),
    }
}

fn parse_privacy_mode() -> Result<PrivacyMode, String> {
    let raw = env("WHOOP_MCP_PRIVACY_MODE").unwrap_or_else(|| "standard".into());
    PrivacyMode::parse(&raw).ok_or_else(|| {
        format!("Invalid WHOOP_MCP_PRIVACY_MODE: \"{raw}\". Must be one of: standard, aggregate.")
    })
}

fn parse_list(raw: Option<String>) -> Vec<String> {
    raw.map(|r| parse_allowed_redirect_uris(&r))
        .unwrap_or_default()
}

/// WHOOP client that authenticates on first use so MCP initialization never
/// blocks on the interactive browser flow. The outcome is memoized.
pub struct DeferredClient {
    config: OAuthConfig,
    logger: Logger,
    cache: Arc<MemoryCache>,
    client: OnceCell<Result<Arc<WhoopClient>, ApiError>>,
}

impl DeferredClient {
    pub fn new(config: OAuthConfig, logger: Logger, cache: Arc<MemoryCache>) -> Self {
        Self {
            config,
            logger,
            cache,
            client: OnceCell::new(),
        }
    }

    fn refresher(&self) -> TokenRefresher {
        let (config, cache, logger) =
            (self.config.clone(), self.cache.clone(), self.logger.clone());
        Arc::new(move || {
            let (config, cache, logger) = (config.clone(), cache.clone(), logger.clone());
            Box::pin(async move {
                let tokens = load_tokens(config.token_dir.as_deref()).ok_or(
                    "Token refresh failed: no stored tokens found. Re-authentication may be required.",
                )?;
                let refreshed = refresh_access_token(&tokens.refresh_token, &config)
                    .await
                    .map_err(|e| e.to_string())?;
                let fresh = to_oauth_tokens(&refreshed, Some(&tokens.refresh_token));
                save_tokens(&fresh, config.token_dir.as_deref()).map_err(|e| e.to_string())?;
                cache.clear();
                logger.info("whoop token refreshed", &[]);
                Ok(fresh.access_token)
            })
        })
    }

    async fn connect(&self) -> Result<Arc<WhoopClient>, ApiError> {
        eprintln!("Authenticating with WHOOP...");
        let access_token = authenticate(&self.config)
            .await
            .map_err(|error| match error {
                OAuthError::Network(message) => ApiError::Network(message),
                OAuthError::Other(message) => ApiError::Other(message),
            })?;
        eprintln!("Authentication successful.");
        self.logger.info("whoop authentication complete", &[]);
        Ok(Arc::new(WhoopClient::new(WhoopClientOptions {
            access_token,
            on_token_refresh: Some(self.refresher()),
            logger: Some(self.logger.clone()),
            cache: Some(self.cache.clone()),
            ..WhoopClientOptions::default()
        })))
    }
}

impl WhoopApi for DeferredClient {
    fn get<'a>(
        &'a self,
        path: &'a str,
        options: GetOptions,
    ) -> BoxFuture<'a, Result<Value, ApiError>> {
        Box::pin(async move {
            let client = self.client.get_or_init(|| self.connect()).await.clone()?;
            client.get(path, options).await
        })
    }
}

/// Transports started by [`serve`].
pub struct Running {
    stdio: Option<JoinHandle<()>>,
    http: Option<HttpServerHandle>,
    logger: Logger,
}

impl Running {
    /// Run until stdio closes (stdio-only) or a shutdown signal arrives (HTTP).
    pub async fn wait(self) {
        match self.http {
            None => {
                if let Some(stdio) = self.stdio {
                    let _ = stdio.await;
                }
            }
            Some(http) => {
                shutdown_signal().await;
                self.logger.info("shutting down", &[]);
                http.close().await;
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Start the MCP server on the configured transports.
pub async fn serve(telemetry: Arc<Telemetry>) -> Result<Running, String> {
    let privacy_mode = parse_privacy_mode()?;
    let transport = parse_transport()?;
    let logger = Logger::new(parse_log_level()?, parse_log_format()?);

    let config = OAuthConfig {
        client_id: required_env("WHOOP_CLIENT_ID")?,
        client_secret: required_env("WHOOP_CLIENT_SECRET")?,
        ..OAuthConfig::default()
    };
    let cache = Arc::new(MemoryCache::default());
    let client: Arc<dyn WhoopApi> = Arc::new(DeferredClient::new(config, logger.clone(), cache));

    let server = Arc::new(McpServer::new(
        client.clone(),
        ServerOptions {
            privacy_mode,
            disable_resources: env("WHOOP_MCP_DISABLE_RESOURCES").as_deref() == Some("1"),
            telemetry: telemetry.enabled().then(|| telemetry.clone()),
            ..ServerOptions::default()
        },
    ));

    let stdio = matches!(transport, TransportMode::Stdio | TransportMode::Both)
        .then(|| tokio::spawn(crate::transport::stdio::serve_stdio(server.clone())));

    let mut http = None;
    if matches!(transport, TransportMode::Http | TransportMode::Both) {
        let auth_token = required_env("MCP_AUTH_TOKEN")?;
        let port = parse_port()?;
        let host = env("MCP_HOST").unwrap_or_else(|| "0.0.0.0".into());
        let allowed_origins = parse_list(env("MCP_ALLOWED_ORIGINS"));
        let trust_proxy = env("MCP_TRUST_PROXY").as_deref() == Some("1");
        let probe = client.clone();
        let health_check: crate::transport::http::HealthCheck = Arc::new(move || {
            let probe = probe.clone();
            Box::pin(async move {
                probe
                    .get("/v2/user/profile/basic", GetOptions::default())
                    .await
                    .is_ok()
            })
        });

        let mut oauth = None;
        let non_empty = |name: &str| env(name).filter(|v| !v.is_empty());
        if let (Some(password), Some(public_url), Some(redirects)) = (
            non_empty("MCP_CONNECTOR_PASSWORD"),
            non_empty("PUBLIC_URL"),
            non_empty("ALLOWED_REDIRECT_URIS"),
        ) {
            let jwt_secret = non_empty("MCP_JWT_SECRET")
                .map_or_else(|| derive_jwt_secret(&auth_token), String::into_bytes);
            let redirect_uris = parse_allowed_redirect_uris(&redirects);
            let connector = OAuthConnector::new(OAuthConnectorOptions {
                connector_password: password,
                public_url: public_url.clone(),
                allowed_redirect_uris: redirect_uris.clone(),
                jwt_secret,
                scopes: vec!["mcp".into()],
                client: ConnectorClientConfig {
                    client_id: env("MCP_OAUTH_CLIENT_ID")
                        .unwrap_or_else(|| "whoop-mcp-connector".into()),
                    client_secret: None,
                    redirect_uris,
                    client_name: Some("WHOOP MCP Connector".into()),
                },
                trust_proxy,
            })?;
            oauth = Some(Arc::new(connector));
            logger.info(
                "oauth connector mounted",
                &[("publicUrl", Value::from(public_url))],
            );
        }

        let oauth_mounted = oauth.is_some();
        let options = HttpServerOptions {
            host: host.clone(),
            allowed_origins: allowed_origins.clone(),
            trust_proxy,
            health_check: Some(health_check),
            oauth,
            ..HttpServerOptions::new(auth_token, port)
        };
        let handle = create_http_server(options, server.clone()).await?;
        logger.info(
            "http transport listening",
            &[
                ("port", Value::from(port)),
                ("host", Value::from(host)),
                ("allowedOriginsCount", Value::from(allowed_origins.len())),
                ("oauthMounted", Value::from(oauth_mounted)),
            ],
        );
        http = Some(handle);
    }

    eprintln!("WHOOP MCP server started on stdio.");
    let transport_name = match transport {
        TransportMode::Stdio => "stdio",
        TransportMode::Http => "http",
        TransportMode::Both => "both",
    };
    logger.info(
        "whoop mcp server started",
        &[("transport", Value::from(transport_name))],
    );
    Ok(Running {
        stdio,
        http,
        logger,
    })
}

const HELP: &str = "Usage: whoop-mcp [command]

Commands:
  (none)              Start the MCP server (MCP_TRANSPORT=stdio|http|both)
  setup [options]     Configure credentials and an MCP client
                      --client <claude-desktop|claude-code|codex|copilot>
                      --client-id <id> --client-secret <secret>
                      --verify --telemetry <on|off>
  doctor [--json]     Local-only configuration diagnostics
  telemetry status    Show telemetry consent status

Options:
  -h, --help          Show this help
  -V, --version       Show the version";

/// Run the CLI and return the process exit code.
pub async fn run_cli(args: Vec<String>) -> i32 {
    let subcommand = args.first().map(String::as_str);
    match subcommand {
        Some("doctor") => return crate::cli::doctor::run_doctor_default(&args[1..]),
        Some("-h" | "--help") => {
            println!("{HELP}");
            return 0;
        }
        Some("-V" | "--version") => {
            println!("whoop-mcp {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        _ => {}
    }

    let is_setup = subcommand == Some("setup");
    let mut telemetry = Arc::new(if is_setup {
        Telemetry::from_env(|key| {
            if key == "WHOOP_MCP_TELEMETRY" {
                Some("0".into())
            } else {
                env(key)
            }
        })
    } else {
        Telemetry::from_process_env()
    });

    if subcommand == Some("telemetry") {
        if args.len() != 2 || args[1] != "status" {
            eprintln!("Usage: whoop-mcp telemetry status");
            return 1;
        }
        println!("{}", telemetry.status());
        return 0;
    }

    let name = if is_setup { "setup" } else { "serve" };
    let outcome: Result<Option<Running>, String> = if is_setup {
        match crate::cli::setup::parse_setup_args(&args[1..]) {
            Ok(options) => match crate::cli::setup::run_setup_default(&options).await {
                Ok(consent) => {
                    telemetry = Arc::new(Telemetry::from_env(|key| {
                        if let Some(value) = consent.get(key).and_then(Value::as_str) {
                            return Some(value.to_string());
                        }
                        if key == "WHOOP_MCP_TELEMETRY" {
                            Some("0".into())
                        } else {
                            env(key)
                        }
                    }));
                    Ok(None)
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    } else {
        serve(telemetry.clone()).await.map(Some)
    };

    let success = outcome.is_ok();
    telemetry.record(&TelemetryEvent::Command {
        name: name.into(),
        success,
    });
    telemetry.flush().await;

    match outcome {
        Ok(Some(running)) => {
            running.wait().await;
            0
        }
        Ok(None) => 0,
        Err(error) => {
            if is_setup {
                eprintln!("Setup failed: {error}");
            } else {
                eprintln!("Fatal error: {error}");
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_matches_javascript() {
        assert_eq!(parse_int("3000"), Some(3000));
        assert_eq!(parse_int(" 42abc"), Some(42));
        assert_eq!(parse_int("-1"), Some(-1));
        assert_eq!(parse_int("abc"), None);
        assert_eq!(parse_int(""), None);
    }
}
