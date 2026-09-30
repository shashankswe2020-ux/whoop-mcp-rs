//! Integration tests for the Streamable HTTP transport and OAuth connector.

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use whoop_mcp::api::{ApiError, BoxFuture, GetOptions, WhoopApi};
use whoop_mcp::crypto::{base64url_encode, sha256};
use whoop_mcp::server::{McpServer, ServerOptions};
use whoop_mcp::transport::http::{HttpServerHandle, HttpServerOptions, create_http_server};
use whoop_mcp::transport::oauth_connector::{
    ConnectorClientConfig, OAuthConnector, OAuthConnectorOptions,
};

const TOKEN: &str = "static-test-token-0123456789";
const PASSWORD: &str = "correct horse battery";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

struct Profile;

impl WhoopApi for Profile {
    fn get<'a>(
        &'a self,
        _path: &'a str,
        _options: GetOptions,
    ) -> BoxFuture<'a, Result<Value, ApiError>> {
        Box::pin(async {
            Ok(json!({"user_id": 1, "email": "a@b.c", "first_name": "A", "last_name": "B"}))
        })
    }
}

fn connector() -> Arc<OAuthConnector> {
    Arc::new(
        OAuthConnector::new(OAuthConnectorOptions {
            connector_password: PASSWORD.into(),
            public_url: "https://mcp.example.com".into(),
            allowed_redirect_uris: vec![REDIRECT.into()],
            jwt_secret: whoop_mcp::transport::oauth_connector::derive_jwt_secret(TOKEN),
            scopes: vec!["mcp".into()],
            client: ConnectorClientConfig {
                client_id: "whoop-mcp-connector".into(),
                client_secret: None,
                redirect_uris: vec![REDIRECT.into()],
                client_name: Some("WHOOP MCP Connector".into()),
            },
            trust_proxy: false,
        })
        .expect("valid connector"),
    )
}

async fn start(configure: impl FnOnce(&mut HttpServerOptions)) -> (HttpServerHandle, String) {
    whoop_mcp::net::install_crypto_provider();
    let mut options = HttpServerOptions::new(TOKEN, 0);
    options.host = "127.0.0.1".into();
    configure(&mut options);
    let mcp = Arc::new(McpServer::new(Arc::new(Profile), ServerOptions::default()));
    let handle = create_http_server(options, mcp)
        .await
        .expect("server starts");
    let base = format!("http://{}", handle.local_addr);
    (handle, base)
}

fn client() -> reqwest::Client {
    whoop_mcp::net::http_client(false)
}

fn mcp_post(base: &str, body: &Value, session: Option<&str>) -> reqwest::RequestBuilder {
    let mut request = client()
        .post(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request
}

fn sse_messages(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("valid SSE JSON"))
        .collect()
}

async fn initialize(base: &str) -> String {
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
    let response = mcp_post(base, &init, None).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let session = response.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let messages = sse_messages(&response.text().await.unwrap());
    assert_eq!(messages[0]["result"]["protocolVersion"], "2025-06-18");
    session
}

#[tokio::test]
async fn health_reports_detail_only_to_authenticated_callers() {
    let (server, base) = start(|_| {}).await;
    let public: Value = client()
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(public, json!({"status": "ok"}));
    let authed: Value = client()
        .get(format!("{base}/health"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(authed["status"], "ok");
    assert_eq!(authed["whoopApi"], "unknown");
    assert!(authed["uptime"].is_u64());
    let missing = client().get(format!("{base}/nope")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    assert_eq!(
        missing.json::<Value>().await.unwrap(),
        json!({"error": "Not Found"})
    );
    server.close().await;
}

#[tokio::test]
async fn rejects_unauthenticated_mcp_requests() {
    let (server, base) = start(|_| {}).await;
    let none = client().post(format!("{base}/mcp")).send().await.unwrap();
    assert_eq!(none.status(), 401);
    let wrong = client()
        .post(format!("{base}/mcp"))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert_eq!(
        wrong.json::<Value>().await.unwrap(),
        json!({"error": "Unauthorized"})
    );
    let basic = client()
        .post(format!("{base}/mcp"))
        .header("Authorization", format!("bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(basic.status(), 401);
    server.close().await;
}

#[tokio::test]
async fn streamable_http_session_lifecycle() {
    let (server, base) = start(|_| {}).await;
    let session = initialize(&base).await;

    let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let accepted = mcp_post(&base, &notification, Some(&session))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 202);

    let batch = json!([
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "get_profile", "arguments": {}}}
    ]);
    let response = mcp_post(&base, &batch, Some(&session))
        .send()
        .await
        .unwrap();
    let mut messages = sse_messages(&response.text().await.unwrap());
    messages.sort_by_key(|m| m["id"].as_i64());
    assert_eq!(messages[0]["result"]["tools"].as_array().unwrap().len(), 16);
    assert_eq!(messages[1]["result"]["structuredContent"]["email"], "a@b.c");

    let missing = mcp_post(
        &base,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}),
        None,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(missing.status(), 400);
    let unknown = mcp_post(
        &base,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}),
        Some("bogus"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(unknown.status(), 404);
    let reinit = mcp_post(
        &base,
        &json!({"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
        Some(&session),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(reinit.status(), 400);

    let bad_version = mcp_post(
        &base,
        &json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}),
        Some(&session),
    )
    .header("mcp-protocol-version", "1999-01-01")
    .send()
    .await
    .unwrap();
    assert_eq!(bad_version.status(), 400);

    let deleted = client()
        .delete(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    let after = mcp_post(
        &base,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        Some(&session),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(after.status(), 404);
    server.close().await;
}

#[tokio::test]
async fn validates_headers_bodies_and_methods() {
    let (server, base) = start(|_| {}).await;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let not_acceptable = client()
        .post(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(not_acceptable.status(), 406);
    let wrong_type = client()
        .post(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "text/plain")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_type.status(), 415);
    let invalid_json = client()
        .post(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_json.status(), 400);
    assert_eq!(
        invalid_json.json::<Value>().await.unwrap(),
        json!({"error": "Bad Request", "message": "Invalid JSON body"})
    );
    let invalid_rpc = mcp_post(&base, &json!({"hello": "world"}), None)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_rpc.status(), 400);
    let put = client()
        .put(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 405);
    assert_eq!(put.headers()["allow"], "GET, POST, DELETE");
    server.close().await;
}

#[tokio::test]
async fn cors_preflight_and_headers() {
    let (server, base) = start(|o| o.allowed_origins = vec!["https://app.example".into()]).await;
    let allowed = client()
        .request(reqwest::Method::OPTIONS, format!("{base}/mcp"))
        .header("Origin", "https://app.example")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 204);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "https://app.example"
    );
    let denied = client()
        .request(reqwest::Method::OPTIONS, format!("{base}/mcp"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    assert!(
        denied
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    server.close().await;
}

#[tokio::test]
async fn enforces_rate_and_connection_limits() {
    let (server, base) = start(|o| o.mcp_rate_limit = (60_000, 2)).await;
    let ping = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    for _ in 0..2 {
        let _ = mcp_post(&base, &ping, None).send().await.unwrap();
    }
    let limited = mcp_post(&base, &ping, None).send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["retry-after"], "60");
    server.close().await;

    let (server, base) = start(|o| {
        o.max_connections = 1;
        o.sse_keepalive = Duration::from_millis(50);
    })
    .await;
    let session = initialize(&base).await;
    let stream = client()
        .get(format!("{base}/mcp"))
        .bearer_auth(TOKEN)
        .header("Accept", "text/event-stream")
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let busy = mcp_post(
        &base,
        &json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}),
        Some(&session),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(busy.status(), 503);
    drop(stream);
    server.close().await;
}

#[tokio::test]
async fn standalone_sse_stream_allows_one_per_session() {
    let (server, base) = start(|o| o.sse_keepalive = Duration::from_millis(20)).await;
    let session = initialize(&base).await;
    let open = || {
        client()
            .get(format!("{base}/mcp"))
            .bearer_auth(TOKEN)
            .header("Accept", "text/event-stream")
            .header("mcp-session-id", &session)
            .send()
    };
    let mut first = open().await.unwrap();
    assert_eq!(first.status(), 200);
    let chunk = tokio::time::timeout(Duration::from_secs(2), first.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&chunk[..], b": keepalive\n\n");
    let second = open().await.unwrap();
    assert_eq!(second.status(), 409);
    server.close().await;
}

fn query_param(url: &str, key: &str) -> Option<String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

#[tokio::test]
async fn oauth_connector_end_to_end() {
    let (server, base) = start(|o| o.oauth = Some(connector())).await;

    let metadata: Value = client()
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["issuer"], "https://mcp.example.com/");
    assert_eq!(metadata["token_endpoint"], "https://mcp.example.com/token");
    let prm: Value = client()
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(prm["authorization_servers"][0], "https://mcp.example.com/");
    assert_eq!(
        client()
            .post(format!("{base}/register"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    let verifier = "a-very-long-code-verifier-string-for-pkce-0123456789";
    let challenge = base64url_encode(&sha256(verifier.as_bytes()));
    let page = client()
        .get(format!(
            "{base}/authorize?client_id=whoop-mcp-connector&redirect_uri={}&response_type=code&code_challenge={challenge}&code_challenge_method=S256&state=xyz%22%3E",
            whoop_mcp::js::encode_uri_component(REDIRECT)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    let html = page.text().await.unwrap();
    assert!(html.contains("name=\"state\" value=\"xyz&quot;&gt;\""));

    let form = |password: &str| {
        whoop_mcp::js::search_params(&[
            ("client_id", "whoop-mcp-connector"),
            ("redirect_uri", REDIRECT),
            ("response_type", "code"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "mcp"),
            ("state", "st"),
            ("connector_password", password),
        ])
    };
    let wrong = client()
        .post(format!("{base}/authorize"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form("wrong password!"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(
        wrong
            .text()
            .await
            .unwrap()
            .contains("Incorrect password. Try again.")
    );

    let approved = client()
        .post(format!("{base}/authorize"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form(PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), 302);
    let location = approved.headers()["location"].to_str().unwrap().to_string();
    assert!(location.starts_with(REDIRECT));
    assert_eq!(query_param(&location, "state").as_deref(), Some("st"));
    let code = query_param(&location, "code").unwrap();

    let token_request = |pairs: Vec<(&str, String)>| {
        let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        client()
            .post(format!("{base}/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(whoop_mcp::js::search_params(&pairs))
            .send()
    };
    let bad_verifier = token_request(vec![
        ("grant_type", "authorization_code".into()),
        ("client_id", "whoop-mcp-connector".into()),
        ("code", code.clone()),
        ("code_verifier", "wrong".into()),
    ])
    .await
    .unwrap();
    assert_eq!(bad_verifier.status(), 400);
    assert_eq!(
        bad_verifier.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );

    let tokens: Value = token_request(vec![
        ("grant_type", "authorization_code".into()),
        ("client_id", "whoop-mcp-connector".into()),
        ("code", code.clone()),
        ("code_verifier", verifier.into()),
        ("redirect_uri", REDIRECT.into()),
    ])
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["scope"], "mcp");
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    let replay = token_request(vec![
        ("grant_type", "authorization_code".into()),
        ("client_id", "whoop-mcp-connector".into()),
        ("code", code),
        ("code_verifier", verifier.into()),
    ])
    .await
    .unwrap();
    assert_eq!(replay.status(), 500);

    let ping = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let with_jwt = client()
        .post(format!("{base}/mcp"))
        .bearer_auth(&access)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(ping.to_string())
        .send()
        .await
        .unwrap();
    assert_ne!(with_jwt.status(), 401);
    let refresh_as_access = client()
        .post(format!("{base}/mcp"))
        .bearer_auth(&refresh)
        .send()
        .await
        .unwrap();
    assert_eq!(refresh_as_access.status(), 401);

    let rotated = token_request(vec![
        ("grant_type", "refresh_token".into()),
        ("client_id", "whoop-mcp-connector".into()),
        ("refresh_token", refresh.clone()),
    ])
    .await
    .unwrap();
    assert_eq!(rotated.status(), 200);
    let reused = token_request(vec![
        ("grant_type", "refresh_token".into()),
        ("client_id", "whoop-mcp-connector".into()),
        ("refresh_token", refresh),
    ])
    .await
    .unwrap();
    assert_eq!(reused.status(), 500);

    let unsupported = token_request(vec![
        ("grant_type", "password".into()),
        ("client_id", "whoop-mcp-connector".into()),
    ])
    .await
    .unwrap();
    assert_eq!(
        unsupported.json::<Value>().await.unwrap()["error"],
        "unsupported_grant_type"
    );
    let bad_client = token_request(vec![
        ("grant_type", "refresh_token".into()),
        ("client_id", "evil".into()),
    ])
    .await
    .unwrap();
    assert_eq!(
        bad_client.json::<Value>().await.unwrap()["error"],
        "invalid_client"
    );
    server.close().await;
}

#[tokio::test]
async fn authorize_rate_limit_and_redirect_validation() {
    let (server, base) = start(|o| o.oauth = Some(connector())).await;
    for _ in 0..3 {
        assert_eq!(
            client()
                .get(format!("{base}/authorize"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let limited = client()
        .get(format!("{base}/authorize"))
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
    server.close().await;

    let (server, base) = start(|o| o.oauth = Some(connector())).await;
    let body = whoop_mcp::js::search_params(&[
        ("client_id", "whoop-mcp-connector"),
        ("redirect_uri", "https://evil.example/cb"),
        ("response_type", "code"),
        ("code_challenge", "c"),
        ("code_challenge_method", "S256"),
        ("connector_password", PASSWORD),
    ]);
    let rejected = client()
        .post(format!("{base}/authorize"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 400);
    assert_eq!(
        rejected.json::<Value>().await.unwrap(),
        json!({"error": "invalid_request", "error_description": "Unregistered redirect_uri"})
    );
    server.close().await;
}

#[test]
fn connector_rejects_weak_configuration() {
    let base = OAuthConnectorOptions {
        connector_password: "short".into(),
        public_url: "https://mcp.example.com".into(),
        allowed_redirect_uris: vec![],
        jwt_secret: vec![1; 32],
        scopes: vec![],
        client: ConnectorClientConfig::default(),
        trust_proxy: false,
    };
    assert!(
        OAuthConnector::new(base.clone())
            .err()
            .unwrap()
            .starts_with("MCP_CONNECTOR_PASSWORD must be at least 12")
    );
    let http = OAuthConnectorOptions {
        connector_password: PASSWORD.into(),
        public_url: "http://mcp.example.com".into(),
        ..base
    };
    assert!(
        OAuthConnector::new(http)
            .err()
            .unwrap()
            .starts_with("PUBLIC_URL must use https://")
    );
}
