//! Temporary loopback HTTP server that receives the OAuth redirect.

use crate::http_util::{Body, full, query_param, request_url, response};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Authorization code and state received on the callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
}

/// Options for [`start_callback_server`].
#[derive(Debug, Clone)]
pub struct CallbackServerOptions {
    pub host: String,
    pub port: u16,
    pub callback_path: String,
    pub expected_state: String,
    pub timeout: Duration,
}

impl CallbackServerOptions {
    /// Defaults: 127.0.0.1:3000/callback with a two minute timeout.
    pub fn new(expected_state: impl Into<String>) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 3000,
            callback_path: "/callback".into(),
            expected_state: expected_state.into(),
            timeout: Duration::from_millis(120_000),
        }
    }
}

/// Handle returned once the server is listening.
pub struct CallbackServerHandle {
    /// Actual port (useful when binding port 0).
    pub port: u16,
    /// Resolves with the callback result or an error message.
    pub result: JoinHandle<Result<CallbackResult, String>>,
}

const SUCCESS_HTML: &str = "<!DOCTYPE html>\n<html><head><title>WHOOP MCP — Success</title></head>\n<body style=\"font-family:system-ui,sans-serif;text-align:center;padding:3rem\">\n<h1>✅ Authentication Successful</h1>\n<p>You can close this window and return to your terminal.</p>\n</body></html>";

const SECURITY_HEADERS: [(&str, &str); 4] = [
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Cache-Control", "no-store"),
    ("Referrer-Policy", "no-referrer"),
];

/// Escape text for HTML embedding.
pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

fn error_html(message: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><title>WHOOP MCP — Error</title></head>\n<body style=\"font-family:system-ui,sans-serif;text-align:center;padding:3rem\">\n<h1>❌ Authentication Failed</h1>\n<p>{}</p>\n</body></html>",
        escape_html(message)
    )
}

fn html(status: u16, body: String) -> Response<Body> {
    let mut headers = vec![("Content-Type", "text/html; charset=utf-8")];
    headers.extend_from_slice(&SECURITY_HEADERS);
    response(status, &headers, full(body))
}

type Outcome = Result<CallbackResult, String>;

fn handle(
    req: &Request<Incoming>,
    port: u16,
    options: &CallbackServerOptions,
) -> (Response<Body>, Option<Outcome>) {
    let Some(url) = request_url(req.uri(), &format!("localhost:{port}")) else {
        return (html(400, error_html("Invalid callback request.")), None);
    };
    if url.path() != options.callback_path {
        let mut headers = vec![("Content-Type", "text/plain; charset=utf-8")];
        headers.extend_from_slice(&SECURITY_HEADERS);
        return (response(404, &headers, full("Not found")), None);
    }
    if let Some(error) = query_param(&url, "error").filter(|e| !e.is_empty()) {
        let description = query_param(&url, "error_description").unwrap_or(error);
        return (
            html(400, error_html(&description)),
            Some(Err(format!("OAuth error: {description}"))),
        );
    }
    let code = query_param(&url, "code").filter(|c| !c.is_empty());
    let state = query_param(&url, "state");
    let Some(code) = code else {
        return (
            html(400, error_html("Missing authorization code in callback.")),
            Some(Err("Missing authorization code in callback".into())),
        );
    };
    if state.as_deref() != Some(options.expected_state.as_str()) {
        let got = state.unwrap_or_else(|| "null".to_string());
        return (
            html(
                400,
                error_html("State parameter mismatch — possible CSRF attack."),
            ),
            Some(Err(format!(
                "State mismatch: expected \"{}\", got \"{got}\"",
                options.expected_state
            ))),
        );
    }
    (
        html(200, SUCCESS_HTML.into()),
        Some(Ok(CallbackResult {
            code,
            state: options.expected_state.clone(),
        })),
    )
}

/// Start the callback server. Binding errors surface through `result`.
pub async fn start_callback_server(options: CallbackServerOptions) -> CallbackServerHandle {
    let listener = match TcpListener::bind((options.host.as_str(), options.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            let message = if error.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "Port {} is already in use. Close the other application and try again.",
                    options.port
                )
            } else {
                format!("Callback server error: {error}")
            };
            return CallbackServerHandle {
                port: options.port,
                result: tokio::spawn(async move { Err(message) }),
            };
        }
    };
    let port = listener.local_addr().map_or(options.port, |a| a.port());
    let options = Arc::new(options);
    let (tx, mut rx) = mpsc::unbounded_channel::<Outcome>();
    let settled = Arc::new(Mutex::new(false));

    let result = tokio::spawn(async move {
        let deadline = tokio::time::sleep(options.timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                outcome = rx.recv() => {
                    return outcome.unwrap_or_else(|| Err("Callback server stopped.".into()));
                }
                () = &mut deadline => {
                    return Err(format!(
                        "OAuth callback timed out after {}ms. No redirect received.",
                        options.timeout.as_millis()
                    ));
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let options = options.clone();
                    let tx = tx.clone();
                    let settled = settled.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |req: Request<Incoming>| {
                            let (resp, outcome) = handle(&req, port, &options);
                            if let Some(outcome) = outcome {
                                let mut done = settled.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                if !*done {
                                    *done = true;
                                    let _ = tx.send(outcome);
                                }
                            }
                            async move { Ok::<_, std::convert::Infallible>(resp) }
                        });
                        let _ = http1::Builder::new()
                            .keep_alive(false)
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    CallbackServerHandle { port, result }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, path: &str) -> (u16, String) {
        crate::net::install_crypto_provider();
        let response = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    fn options(state: &str) -> CallbackServerOptions {
        CallbackServerOptions {
            port: 0,
            ..CallbackServerOptions::new(state)
        }
    }

    #[tokio::test]
    async fn resolves_with_code_on_valid_callback() {
        let handle = start_callback_server(options("s1")).await;
        let (status, body) = get(handle.port, "/other").await;
        assert_eq!((status, body.as_str()), (404, "Not found"));
        let (status, body) = get(handle.port, "/callback?code=abc&state=s1").await;
        assert_eq!(status, 200);
        assert!(body.contains("Authentication Successful"));
        assert_eq!(
            handle.result.await.unwrap(),
            Ok(CallbackResult {
                code: "abc".into(),
                state: "s1".into()
            })
        );
    }

    #[tokio::test]
    async fn rejects_state_mismatch_and_escapes_errors() {
        let handle = start_callback_server(options("s1")).await;
        let (status, _) = get(handle.port, "/callback?code=abc&state=evil").await;
        assert_eq!(status, 400);
        assert_eq!(
            handle.result.await.unwrap(),
            Err("State mismatch: expected \"s1\", got \"evil\"".into())
        );

        let handle = start_callback_server(options("s1")).await;
        let (status, body) = get(
            handle.port,
            "/callback?error=access_denied&error_description=%3Cscript%3E",
        )
        .await;
        assert_eq!(status, 400);
        assert!(body.contains("&lt;script&gt;"));
        assert_eq!(
            handle.result.await.unwrap(),
            Err("OAuth error: <script>".into())
        );
    }

    #[tokio::test]
    async fn times_out_and_reports_port_conflicts() {
        let mut opts = options("s");
        opts.timeout = Duration::from_millis(10);
        let handle = start_callback_server(opts).await;
        assert_eq!(
            handle.result.await.unwrap(),
            Err("OAuth callback timed out after 10ms. No redirect received.".into())
        );

        let blocker = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = blocker.local_addr().unwrap().port();
        let handle = start_callback_server(CallbackServerOptions {
            port,
            ..CallbackServerOptions::new("s")
        })
        .await;
        assert_eq!(
            handle.result.await.unwrap(),
            Err(format!(
                "Port {port} is already in use. Close the other application and try again."
            ))
        );
    }
}
