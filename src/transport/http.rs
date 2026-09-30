//! Remote HTTP transport: bearer-authenticated Streamable HTTP (`/mcp`),
//! health checks, CORS, rate and connection limits, and the optional OAuth
//! connector sharing the same port.

use super::oauth_connector::{ConnectorRequest, OAuthConnector};
use crate::api::BoxFuture;
use crate::crypto::{safe_token_compare, uuid_v4};
use crate::http_util::{Body, ChannelBody, GuardedBody, empty, json_response, response};
use crate::server::{
    McpServer, MessageKind, SUPPORTED_PROTOCOL_VERSIONS, classify, is_initialize_request,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::HeaderMap;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_FORM_BYTES: usize = 100 * 1024;
const MAX_SESSIONS: usize = 64;

/// Async probe of upstream WHOOP reachability.
pub type HealthCheck = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

/// HTTP server configuration.
#[derive(Clone)]
pub struct HttpServerOptions {
    pub auth_token: String,
    pub port: u16,
    pub host: String,
    pub max_connections: usize,
    pub allowed_origins: Vec<String>,
    pub trust_proxy: bool,
    pub health_check: Option<HealthCheck>,
    pub oauth: Option<Arc<OAuthConnector>>,
    /// Per-IP fixed window for `/mcp`: (window ms, max requests). Zero disables.
    pub mcp_rate_limit: (u64, u64),
    /// Re-validate SSE bearer tokens on this interval (zero disables).
    pub sse_reauth_interval: Duration,
    pub sse_keepalive: Duration,
}

impl HttpServerOptions {
    /// Defaults matching the TypeScript server.
    pub fn new(auth_token: impl Into<String>, port: u16) -> Self {
        Self {
            auth_token: auth_token.into(),
            port,
            host: "0.0.0.0".into(),
            max_connections: 5,
            allowed_origins: Vec::new(),
            trust_proxy: false,
            health_check: None,
            oauth: None,
            mcp_rate_limit: (60_000, 100),
            sse_reauth_interval: Duration::from_secs(5 * 60),
            sse_keepalive: Duration::from_secs(15),
        }
    }
}

#[derive(Clone)]
struct SseSender(mpsc::UnboundedSender<Bytes>);

impl SseSender {
    fn event(&self, message: &Value) -> bool {
        let frame = format!(
            "event: message\ndata: {}\n\n",
            crate::js::stringify(message)
        );
        self.0.send(Bytes::from(frame)).is_ok()
    }

    fn close(&self) {
        let _ = self.0.send(Bytes::new());
    }
}

struct Session {
    standalone: Option<(u64, SseSender)>,
    last_used: Instant,
}

struct SseEntry {
    token: String,
    stream: SseSender,
}

struct State {
    options: HttpServerOptions,
    mcp: Arc<McpServer>,
    started: Instant,
    active: AtomicUsize,
    next_id: AtomicU64,
    rate: Mutex<HashMap<String, (u64, Instant)>>,
    sessions: Mutex<HashMap<String, Session>>,
    sse: Mutex<HashMap<u64, SseEntry>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Releases an `/mcp` connection slot (and SSE registrations) when the response body is dropped.
struct ConnectionGuard {
    state: Arc<State>,
    sse_id: Option<u64>,
    standalone: Option<(String, u64)>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(id) = self.sse_id {
            lock(&self.state.sse).remove(&id);
        }
        if let Some((session_id, stream_id)) = &self.standalone
            && let Some(session) = lock(&self.state.sessions).get_mut(session_id)
            && session
                .standalone
                .as_ref()
                .is_some_and(|(id, _)| id == stream_id)
        {
            session.standalone = None;
        }
    }
}

/// A running HTTP server.
pub struct HttpServerHandle {
    pub local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    state: Arc<State>,
}

impl HttpServerHandle {
    /// Stop accepting, close SSE streams, and drain connections.
    pub async fn close(self) {
        for session in lock(&self.state.sessions).drain() {
            if let Some((_, stream)) = session.1.standalone {
                stream.close();
            }
        }
        for (_, entry) in lock(&self.state.sse).drain() {
            entry.stream.close();
        }
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn header_joined(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = header(headers, "authorization")?;
    let parts: Vec<&str> = value.split(' ').collect();
    match parts[..] {
        ["Bearer", token] => Some(token.to_string()),
        _ => None,
    }
}

fn rpc_error(status: u16, code: i64, message: &str, extra: &[(&str, &str)]) -> Response<Body> {
    let body = json!({"jsonrpc": "2.0", "error": {"code": code, "message": message}, "id": null});
    let mut headers = vec![("Content-Type", "application/json")];
    headers.extend_from_slice(extra);
    response(status, &headers, crate::http_util::full(body.to_string()))
}

fn is_json_content_type(value: Option<&str>) -> bool {
    match value {
        Some("application/json") => true,
        Some(v) => v
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json"),
        None => false,
    }
}

impl State {
    fn authenticate(&self, token: &str) -> bool {
        safe_token_compare(token, &self.options.auth_token)
            || self
                .options
                .oauth
                .as_ref()
                .is_some_and(|oauth| oauth.verify_access_token(token).is_ok())
    }

    fn check_rate_limit(&self, ip: &str) -> bool {
        let (window_ms, max) = self.options.mcp_rate_limit;
        if max == 0 || window_ms == 0 {
            return true;
        }
        let now = Instant::now();
        let mut buckets = lock(&self.rate);
        buckets.retain(|_, (_, reset)| *reset > now);
        let bucket = buckets
            .entry(ip.to_string())
            .or_insert((0, now + Duration::from_millis(window_ms)));
        if bucket.0 >= max {
            return false;
        }
        bucket.0 += 1;
        true
    }

    fn client_ip(&self, headers: &HeaderMap, peer: SocketAddr) -> String {
        if self.options.trust_proxy
            && let Some(first) = header(headers, "x-forwarded-for")
                .and_then(|x| x.split(',').next())
                .map(str::trim)
            && !first.is_empty()
        {
            return first.to_string();
        }
        peer.ip().to_string()
    }

    #[allow(clippy::result_large_err)]
    fn validate_session(&self, headers: &HeaderMap) -> Result<String, Response<Body>> {
        let mut sessions = lock(&self.sessions);
        let Some(id) = header(headers, "mcp-session-id") else {
            let message = if sessions.is_empty() {
                "Bad Request: Server not initialized"
            } else {
                "Bad Request: Mcp-Session-Id header is required"
            };
            return Err(rpc_error(400, -32000, message, &[]));
        };
        match sessions.get_mut(id) {
            Some(session) => {
                session.last_used = Instant::now();
                Ok(id.to_string())
            }
            None => Err(rpc_error(404, -32001, "Session not found", &[])),
        }
    }

    #[allow(clippy::result_large_err)]
    fn validate_protocol_version(headers: &HeaderMap) -> Result<(), Response<Body>> {
        match header(headers, "mcp-protocol-version") {
            Some(version) if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => Err(rpc_error(
                400,
                -32000,
                &format!(
                    "Bad Request: Unsupported protocol version: {version} (supported versions: {})",
                    SUPPORTED_PROTOCOL_VERSIONS.join(", ")
                ),
                &[],
            )),
            _ => Ok(()),
        }
    }

    fn create_session(&self) -> String {
        let id = uuid_v4();
        let mut sessions = lock(&self.sessions);
        while sessions.len() >= MAX_SESSIONS {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(session) = sessions.remove(&oldest)
                && let Some((_, stream)) = session.standalone
            {
                stream.close();
            }
        }
        sessions.insert(
            id.clone(),
            Session {
                standalone: None,
                last_used: Instant::now(),
            },
        );
        id
    }

    fn sse_stream(self: &Arc<Self>) -> (SseSender, Body) {
        let (tx, body) = ChannelBody::channel();
        let sender = SseSender(tx);
        let keepalive = sender.clone();
        let interval = self.options.sse_keepalive;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if keepalive
                    .0
                    .send(Bytes::from_static(b": keepalive\n\n"))
                    .is_err()
                {
                    break;
                }
            }
        });
        (sender, body)
    }
}

fn sse_headers(session_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", "text/event-stream".into()),
        ("Cache-Control", "no-cache, no-transform".into()),
        ("Connection", "keep-alive".into()),
        ("X-Accel-Buffering", "no".into()),
        ("mcp-session-id", session_id.into()),
    ]
}

fn with_owned_headers(
    status: u16,
    headers: &[(&'static str, String)],
    body: Body,
) -> Response<Body> {
    let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    response(status, &refs, body)
}

async fn handle_post(state: &Arc<State>, headers: &HeaderMap, body: Value) -> Response<Body> {
    let accept = header_joined(headers, "accept").unwrap_or_default();
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return rpc_error(
            406,
            -32000,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
            &[],
        );
    }
    if !is_json_content_type(header(headers, "content-type")) {
        return rpc_error(
            415,
            -32000,
            "Unsupported Media Type: Content-Type must be application/json",
            &[],
        );
    }
    let messages = match body {
        Value::Array(items) => items,
        single => vec![single],
    };
    if messages.iter().any(|m| classify(m) == MessageKind::Invalid) {
        return rpc_error(400, -32700, "Parse error: Invalid JSON-RPC message", &[]);
    }

    let session_id = if messages.iter().any(is_initialize_request) {
        if header(headers, "mcp-session-id")
            .is_some_and(|id| lock(&state.sessions).contains_key(id))
        {
            return rpc_error(
                400,
                -32600,
                "Invalid Request: Server already initialized",
                &[],
            );
        }
        if messages.len() > 1 {
            return rpc_error(
                400,
                -32600,
                "Invalid Request: Only one initialization request is allowed",
                &[],
            );
        }
        state.create_session()
    } else {
        let id = match state.validate_session(headers) {
            Ok(id) => id,
            Err(resp) => return resp,
        };
        if let Err(resp) = State::validate_protocol_version(headers) {
            return resp;
        }
        id
    };

    let has_requests = messages
        .iter()
        .any(|m| matches!(classify(m), MessageKind::Request { .. }));
    if !has_requests {
        for message in messages {
            let mcp = state.mcp.clone();
            let scope = session_id.clone();
            tokio::spawn(async move {
                mcp.handle_scoped(&scope, &message).await;
            });
        }
        return response(202, &[], empty());
    }

    let (stream, body) = state.sse_stream();
    let mcp = state.mcp.clone();
    let scope = session_id.clone();
    tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        for message in messages {
            let mcp = mcp.clone();
            let scope = scope.clone();
            let stream = stream.clone();
            tasks.spawn(async move {
                if let Some(reply) = mcp.handle_scoped(&scope, &message).await {
                    stream.event(&reply);
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        stream.close();
    });
    with_owned_headers(200, &sse_headers(&session_id), body)
}

fn handle_get(
    state: &Arc<State>,
    headers: &HeaderMap,
    guard: &mut ConnectionGuard,
) -> Response<Body> {
    if !header_joined(headers, "accept")
        .unwrap_or_default()
        .contains("text/event-stream")
    {
        return rpc_error(
            406,
            -32000,
            "Not Acceptable: Client must accept text/event-stream",
            &[],
        );
    }
    let session_id = match state.validate_session(headers) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = State::validate_protocol_version(headers) {
        return resp;
    }
    let (stream, body) = state.sse_stream();
    let stream_id = state.next_id.fetch_add(1, Ordering::SeqCst);
    {
        let mut sessions = lock(&state.sessions);
        let Some(session) = sessions.get_mut(&session_id) else {
            return rpc_error(404, -32001, "Session not found", &[]);
        };
        if session.standalone.is_some() {
            return rpc_error(
                409,
                -32000,
                "Conflict: Only one SSE stream is allowed per session",
                &[],
            );
        }
        session.standalone = Some((stream_id, stream.clone()));
    }
    guard.standalone = Some((session_id.clone(), stream_id));
    if let Some(sse_id) = guard.sse_id
        && let Some(entry) = lock(&state.sse).get_mut(&sse_id)
    {
        entry.stream = stream;
    }
    with_owned_headers(200, &sse_headers(&session_id), body)
}

fn handle_delete(state: &Arc<State>, headers: &HeaderMap) -> Response<Body> {
    let session_id = match state.validate_session(headers) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = State::validate_protocol_version(headers) {
        return resp;
    }
    if let Some(session) = lock(&state.sessions).remove(&session_id)
        && let Some((_, stream)) = session.standalone
    {
        stream.close();
    }
    response(200, &[], empty())
}

async fn handle_mcp(
    state: &Arc<State>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Response<Body> {
    let headers = req.headers().clone();
    let Some(token) = extract_bearer(&headers) else {
        return json_response(401, &json!({"error": "Unauthorized"}), &[]);
    };
    if !state.authenticate(&token) {
        return json_response(401, &json!({"error": "Unauthorized"}), &[]);
    }
    if !state.check_rate_limit(&state.client_ip(&headers, peer)) {
        let retry = state.options.mcp_rate_limit.0.div_ceil(1000).to_string();
        return json_response(
            429,
            &json!({"error": "Too Many Requests"}),
            &[("Retry-After", retry.as_str())],
        );
    }
    if state.active.load(Ordering::SeqCst) >= state.options.max_connections {
        return json_response(
            503,
            &json!({"error": "Service Unavailable", "message": "Maximum connections reached"}),
            &[],
        );
    }
    state.active.fetch_add(1, Ordering::SeqCst);
    let mut guard = ConnectionGuard {
        state: state.clone(),
        sse_id: None,
        standalone: None,
    };
    let method = req.method().as_str().to_string();
    if method == "GET" {
        let id = state.next_id.fetch_add(1, Ordering::SeqCst);
        let (placeholder, _) = mpsc::unbounded_channel();
        lock(&state.sse).insert(
            id,
            SseEntry {
                token: token.clone(),
                stream: SseSender(placeholder),
            },
        );
        guard.sse_id = Some(id);
    }

    let resp = match method.as_str() {
        "POST" => {
            let collected = http_body_util::Limited::new(req.into_body(), MAX_BODY_BYTES)
                .collect()
                .await;
            let parsed = collected
                .ok()
                .and_then(|body| serde_json::from_slice::<Value>(&body.to_bytes()).ok());
            match parsed {
                Some(body) => handle_post(state, &headers, body).await,
                None => json_response(
                    400,
                    &json!({"error": "Bad Request", "message": "Invalid JSON body"}),
                    &[],
                ),
            }
        }
        "GET" => handle_get(state, &headers, &mut guard),
        "DELETE" => handle_delete(state, &headers),
        _ => rpc_error(
            405,
            -32000,
            "Method not allowed.",
            &[("Allow", "GET, POST, DELETE")],
        ),
    };
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, GuardedBody::wrap(body, guard))
}

async fn route(state: Arc<State>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
    let host = header(req.headers(), "host")
        .unwrap_or("localhost")
        .to_string();
    let Some(url) = crate::http_util::request_url(req.uri(), &host) else {
        return json_response(
            400,
            &json!({"error": "Bad Request", "message": "Invalid request URL or Host header"}),
            &[],
        );
    };
    let path = url.path().to_string();

    let origin = header(req.headers(), "origin").map(str::to_string);
    let allowed = origin
        .as_ref()
        .is_some_and(|o| state.options.allowed_origins.contains(o));
    let cors: Vec<(&'static str, String)> = match (&origin, allowed) {
        (Some(origin), true) => vec![
            ("Access-Control-Allow-Origin", origin.clone()),
            (
                "Access-Control-Allow-Methods",
                "GET, POST, DELETE, OPTIONS".into(),
            ),
            (
                "Access-Control-Allow-Headers",
                "Authorization, Content-Type, Mcp-Session-Id".into(),
            ),
            ("Access-Control-Expose-Headers", "Mcp-Session-Id".into()),
            ("Access-Control-Max-Age", "86400".into()),
        ],
        _ => Vec::new(),
    };
    let apply_cors = |mut resp: Response<Body>| {
        for (name, value) in &cors {
            if let Ok(value) = hyper::header::HeaderValue::from_str(value) {
                resp.headers_mut().insert(*name, value);
            }
        }
        resp
    };

    if req.method() == hyper::Method::OPTIONS {
        return apply_cors(response(if allowed { 204 } else { 403 }, &[], empty()));
    }

    if path == "/health" {
        let authed = extract_bearer(req.headers())
            .is_some_and(|t| safe_token_compare(&t, &state.options.auth_token));
        let mut health = json!({"status": "ok"});
        if authed {
            health["uptime"] = Value::from(state.started.elapsed().as_secs());
            health["whoopApi"] = Value::from(match &state.options.health_check {
                Some(check) => {
                    if check().await {
                        "ok"
                    } else {
                        "error"
                    }
                }
                None => "unknown",
            });
        }
        return apply_cors(json_response(200, &health, &[]));
    }

    if let Some(oauth) = &state.options.oauth
        && (matches!(path.as_str(), "/authorize" | "/token" | "/register")
            || path.starts_with("/.well-known/"))
    {
        let method = req.method().as_str().to_string();
        let query = url.query().map(str::to_string);
        let content_type = header(req.headers(), "content-type").map(str::to_string);
        let forwarded = header(req.headers(), "x-forwarded-for").map(str::to_string);
        let body = match http_body_util::Limited::new(req.into_body(), MAX_FORM_BYTES)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                return apply_cors(response(
                    413,
                    &[("Content-Type", "text/plain; charset=utf-8")],
                    crate::http_util::full("request entity too large"),
                ));
            }
        };
        let remote = peer.ip().to_string();
        let request = ConnectorRequest {
            method: &method,
            path: &path,
            query: query.as_deref(),
            content_type: content_type.as_deref(),
            body: &body,
            remote_ip: &remote,
            forwarded_for: forwarded.as_deref(),
        };
        return apply_cors(oauth.handle(&request));
    }

    if path == "/mcp" {
        return apply_cors(handle_mcp(&state, req, peer).await);
    }

    apply_cors(json_response(404, &json!({"error": "Not Found"}), &[]))
}

/// Bind and serve. Fails when `auth_token` is empty or the address cannot be bound.
pub async fn create_http_server(
    options: HttpServerOptions,
    mcp: Arc<McpServer>,
) -> Result<HttpServerHandle, String> {
    if options.auth_token.is_empty() {
        return Err("MCP_AUTH_TOKEN is required when MCP_TRANSPORT=http or MCP_TRANSPORT=both. Set it to a secure random string (32+ characters recommended).".into());
    }
    let listener = TcpListener::bind((options.host.as_str(), options.port))
        .await
        .map_err(|e| format!("Failed to listen on {}:{}: {e}", options.host, options.port))?;
    let local_addr = listener.local_addr().map_err(|e| e.to_string())?;
    let state = Arc::new(State {
        options,
        mcp,
        started: Instant::now(),
        active: AtomicUsize::new(0),
        next_id: AtomicU64::new(1),
        rate: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        sse: Mutex::new(HashMap::new()),
    });
    let (shutdown, shutdown_rx) = watch::channel(false);

    if !state.options.sse_reauth_interval.is_zero() {
        let sweep_state = Arc::downgrade(&state);
        let interval = state.options.sse_reauth_interval;
        let mut stop = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    _ = stop.changed() => break,
                }
                let Some(state) = sweep_state.upgrade() else {
                    break;
                };
                let entries: Vec<(u64, String)> = lock(&state.sse)
                    .iter()
                    .map(|(id, e)| (*id, e.token.clone()))
                    .collect();
                for (id, token) in entries {
                    if !state.authenticate(&token)
                        && let Some(entry) = lock(&state.sse).remove(&id)
                    {
                        entry.stream.close();
                    }
                }
            }
        });
    }

    let accept_state = state.clone();
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        let mut stop = shutdown_rx.clone();
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else { continue };
                    let state = accept_state.clone();
                    let mut conn_stop = shutdown_rx.clone();
                    connections.spawn(async move {
                        let service = service_fn(move |req| {
                            let state = state.clone();
                            async move { Ok::<_, std::convert::Infallible>(route(state, req, peer).await) }
                        });
                        let conn = http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(Duration::from_secs(60))
                            .serve_connection(TokioIo::new(stream), service);
                        tokio::pin!(conn);
                        tokio::select! {
                            _ = conn.as_mut() => {}
                            _ = conn_stop.changed() => {
                                conn.as_mut().graceful_shutdown();
                                let _ = conn.await;
                            }
                        }
                    });
                    while connections.try_join_next().is_some() {}
                }
            }
        }
        let drain = async { while connections.join_next().await.is_some() {} };
        if tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .is_err()
        {
            connections.abort_all();
        }
    });

    Ok(HttpServerHandle {
        local_addr,
        shutdown,
        task,
        state,
    })
}
