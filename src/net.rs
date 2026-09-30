//! Shared outbound HTTP client configuration (rustls with the `ring` provider).

use std::sync::OnceLock;

/// Install the process-wide rustls crypto provider (idempotent).
pub fn install_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build an HTTP client; `follow_redirects` mirrors `fetch`'s default redirect mode.
pub fn http_client(follow_redirects: bool) -> reqwest::Client {
    install_crypto_provider();
    let policy = if follow_redirects {
        reqwest::redirect::Policy::limited(20)
    } else {
        reqwest::redirect::Policy::none()
    };
    reqwest::Client::builder()
        .redirect(policy)
        .user_agent(concat!("whoop-mcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Shared client for WHOOP API and OAuth calls.
pub fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| http_client(true)).clone()
}
