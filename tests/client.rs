//! WHOOP API client and OAuth token endpoint behavior against a local mock server.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use whoop_mcp::api::client::{WhoopClient, WhoopClientOptions};
use whoop_mcp::api::{ApiError, GetOptions, WhoopApi};
use whoop_mcp::auth::oauth::{
    OAuthConfig, OAuthError, exchange_code_for_tokens, refresh_access_token,
};
use whoop_mcp::cache::MemoryCache;

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    authorization: String,
    body: String,
}

struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Scripted `(status, headers, body)` response.
type Scripted = (u16, Vec<(&'static str, &'static str)>, String);

/// Serve scripted responses in order (the last one repeats).
async fn mock(responses: Vec<Scripted>) -> Mock {
    whoop_mcp::net::install_crypto_provider();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_server = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let queue = queue.clone();
            let seen = seen_server.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let queue = queue.clone();
                    let seen = seen.clone();
                    async move {
                        let path = req
                            .uri()
                            .path_and_query()
                            .map(|p| p.to_string())
                            .unwrap_or_default();
                        let authorization = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body = String::from_utf8_lossy(
                            &req.into_body().collect().await.unwrap().to_bytes(),
                        )
                        .into_owned();
                        seen.lock().unwrap().push(Seen {
                            path,
                            authorization,
                            body,
                        });
                        let (status, headers, body) = {
                            let mut queue = queue.lock().unwrap();
                            if queue.len() > 1 {
                                queue.pop_front().unwrap()
                            } else {
                                queue.front().cloned().unwrap()
                            }
                        };
                        let mut response = Response::builder().status(status);
                        for (name, value) in headers {
                            response = response.header(name, value);
                        }
                        Ok::<_, std::convert::Infallible>(
                            response.body(Full::new(Bytes::from(body))).unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Mock { base, seen }
}

fn client(
    base: &str,
    refresher: Option<whoop_mcp::api::client::TokenRefresher>,
    cache: Option<Arc<MemoryCache>>,
) -> WhoopClient {
    WhoopClient::new(WhoopClientOptions {
        access_token: "old-token".into(),
        base_url: Some(base.into()),
        on_token_refresh: refresher,
        cache,
        ..WhoopClientOptions::default()
    })
}

#[tokio::test]
async fn sends_bearer_token_and_parses_json() {
    let server = mock(vec![(200, vec![], json!({"user_id": 1}).to_string())]).await;
    let value = client(&server.base, None, None)
        .get("/v2/user/profile/basic", GetOptions::default())
        .await
        .unwrap();
    assert_eq!(value, json!({"user_id": 1}));
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen[0].path, "/v2/user/profile/basic");
    assert_eq!(seen[0].authorization, "Bearer old-token");
}

#[tokio::test]
async fn retries_rate_limits_using_retry_after() {
    let server = mock(vec![
        (429, vec![("retry-after", "0")], "slow down".into()),
        (429, vec![("retry-after", "0")], "{}".into()),
        (200, vec![], "{\"ok\":true}".into()),
    ])
    .await;
    let value = client(&server.base, None, None)
        .get("/v2/cycle", GetOptions::default())
        .await
        .unwrap();
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(server.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn gives_up_after_three_rate_limit_retries() {
    let server = mock(vec![(
        429,
        vec![("retry-after", "0")],
        "{\"error\":\"busy\"}".into(),
    )])
    .await;
    let error = client(&server.base, None, None)
        .get("/v2/cycle", GetOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ApiError::Api {
            status: 429,
            status_text: "Too Many Requests".into(),
            body: json!({"error": "busy"})
        }
    );
    assert_eq!(server.seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn refreshes_once_on_unauthorized() {
    let server = mock(vec![
        (401, vec![], "{}".into()),
        (200, vec![], "{\"ok\":1}".into()),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let refresher: whoop_mcp::api::client::TokenRefresher = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("new-token".to_string()) })
    });
    let whoop = client(&server.base, Some(refresher), None);
    assert_eq!(
        whoop
            .get("/v2/recovery", GetOptions::default())
            .await
            .unwrap(),
        json!({"ok": 1})
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen[1].authorization, "Bearer new-token");
    whoop
        .get("/v2/recovery", GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        server.seen.lock().unwrap()[2].authorization,
        "Bearer new-token"
    );
}

#[tokio::test]
async fn refresh_failures_become_auth_errors() {
    let server = mock(vec![(401, vec![], "{}".into())]).await;
    let refresher: whoop_mcp::api::client::TokenRefresher =
        Arc::new(|| Box::pin(async { Err("refresh token revoked".to_string()) }));
    let error = client(&server.base, Some(refresher), None)
        .get("/v2/cycle", GetOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error, ApiError::Auth("refresh token revoked".into()));
    assert_eq!(
        error.to_string(),
        "Authentication error: Failed to refresh token. Re-authentication may be required."
    );
}

#[tokio::test]
async fn other_errors_are_not_retried_and_network_failures_are_typed() {
    let server = mock(vec![(500, vec![], "oops".into())]).await;
    let error = client(&server.base, None, None)
        .get("/v2/cycle", GetOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ApiError::Api {
            status: 500,
            status_text: "Internal Server Error".into(),
            body: json!("oops")
        }
    );
    assert_eq!(server.seen.lock().unwrap().len(), 1);

    let unreachable = client("http://127.0.0.1:9", None, None)
        .get("/v2/cycle", GetOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(unreachable, ApiError::Network(_)));
}

#[tokio::test]
async fn caches_opt_in_requests_by_sorted_query() {
    let server = mock(vec![(200, vec![], "{\"records\":[]}".into())]).await;
    let cache = Arc::new(MemoryCache::default());
    let whoop = client(&server.base, None, Some(cache.clone()));
    whoop
        .get("/v2/cycle?limit=1&a=2", GetOptions::cached(60_000))
        .await
        .unwrap();
    whoop
        .get("/v2/cycle?a=2&limit=1", GetOptions::cached(60_000))
        .await
        .unwrap();
    whoop
        .get("/v2/cycle?a=2&limit=1", GetOptions::default())
        .await
        .unwrap();
    assert_eq!(server.seen.lock().unwrap().len(), 2);
    assert_eq!(cache.len(), 1);
}

fn oauth_config(base: &str) -> OAuthConfig {
    OAuthConfig {
        client_id: "id".into(),
        client_secret: "secret".into(),
        redirect_uri: Some("http://localhost:3000/callback".into()),
        token_url: Some(format!("{base}/oauth/oauth2/token")),
        ..OAuthConfig::default()
    }
}

#[tokio::test]
async fn exchanges_codes_and_refreshes_tokens() {
    let tokens = json!({"access_token": "a", "refresh_token": "r", "expires_in": 3600, "token_type": "bearer", "scope": "offline"});
    let server = mock(vec![(200, vec![], tokens.to_string())]).await;
    let config = oauth_config(&server.base);
    let exchanged = exchange_code_for_tokens("the code", &config, Some("verifier"))
        .await
        .unwrap();
    assert_eq!(exchanged.access_token, "a");
    let refreshed = refresh_access_token("r", &config).await.unwrap();
    assert_eq!(refreshed.refresh_token, "r");
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(
        seen[0].body,
        "grant_type=authorization_code&code=the+code&client_id=id&client_secret=secret&redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fcallback&code_verifier=verifier"
    );
    assert_eq!(
        seen[1].body,
        "grant_type=refresh_token&refresh_token=r&client_id=id&client_secret=secret&scope=offline"
    );
}

#[tokio::test]
async fn token_endpoint_errors_are_descriptive() {
    let server = mock(vec![(
        400,
        vec![],
        json!({"error_description": "bad code"}).to_string(),
    )])
    .await;
    let config = oauth_config(&server.base);
    let error = exchange_code_for_tokens("c", &config, None)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        OAuthError::Other("Token exchange failed (400): bad code".into())
    );
    let error = refresh_access_token("r", &config).await.unwrap_err();
    assert_eq!(
        error,
        OAuthError::Other("Token refresh failed (400): bad code".into())
    );
    let offline = oauth_config("http://127.0.0.1:9");
    assert!(matches!(
        refresh_access_token("r", &offline).await.unwrap_err(),
        OAuthError::Network(_)
    ));
}

#[tokio::test]
async fn stdio_transport_round_trips_json_rpc_lines() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use whoop_mcp::server::{McpServer, ServerOptions};
    let server = mock(vec![(200, vec![], "{}".into())]).await;
    let mcp = Arc::new(McpServer::new(
        Arc::new(client(&server.base, None, None)),
        ServerOptions::default(),
    ));
    let (mut input_writer, input_reader) = tokio::io::duplex(4096);
    let (output_writer, mut output_reader) = tokio::io::duplex(65536);
    let task = tokio::spawn(whoop_mcp::transport::stdio::serve(
        mcp,
        input_reader,
        output_writer,
    ));
    input_writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\nnot json\n\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    drop(input_writer);
    task.await.unwrap();
    let mut output = String::new();
    output_reader.read_to_string(&mut output).await.unwrap();
    let lines: Vec<Value> = output
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines,
        vec![json!({"result": {}, "jsonrpc": "2.0", "id": 1})]
    );
}
