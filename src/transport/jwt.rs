//! HS256 JWTs for OAuth connector access and refresh tokens.

use crate::crypto::{base64url_decode, base64url_encode, hmac_sha256, hmac_sha256_verify};
use serde_json::{Map, Value, json};

/// Access token lifetime (24 hours).
pub const ACCESS_TOKEN_TTL_SECONDS: u64 = 24 * 60 * 60;
/// Refresh token lifetime (30 days).
pub const REFRESH_TOKEN_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
/// Issuer claim.
pub const JWT_ISSUER: &str = "whoop-mcp";

/// Token kind (`typ` claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Access,
    Refresh,
}

impl TokenType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Refresh => "refresh",
        }
    }
}

/// Claims to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignOptions {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub resource: Option<String>,
    pub ttl_seconds: u64,
    pub token_type: TokenType,
    pub jti: Option<String>,
}

/// Verified token claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub token_type: TokenType,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub resource: Option<String>,
    /// Seconds since epoch.
    pub expires_at: i64,
    pub jti: Option<String>,
}

fn now_seconds() -> i64 {
    (crate::logging::now_ms() / 1000.0).floor() as i64
}

/// Sign a compact JWT.
pub fn sign_token(options: &SignOptions, secret: &[u8]) -> String {
    sign_token_at(options, secret, now_seconds())
}

fn sign_token_at(options: &SignOptions, secret: &[u8], now: i64) -> String {
    let mut payload = Map::new();
    payload.insert("typ".into(), Value::from(options.token_type.as_str()));
    payload.insert("sub".into(), Value::from(options.client_id.clone()));
    payload.insert("scope".into(), Value::from(options.scopes.join(" ")));
    if let Some(resource) = options.resource.as_ref().filter(|r| !r.is_empty()) {
        payload.insert("resource".into(), Value::from(resource.clone()));
    }
    if let Some(jti) = options.jti.as_ref().filter(|j| !j.is_empty()) {
        payload.insert("jti".into(), Value::from(jti.clone()));
    }
    payload.insert("iss".into(), Value::from(JWT_ISSUER));
    payload.insert("iat".into(), Value::from(now));
    payload.insert("exp".into(), Value::from(now + options.ttl_seconds as i64));
    let header = base64url_encode(json!({"alg": "HS256"}).to_string().as_bytes());
    let body = base64url_encode(Value::Object(payload).to_string().as_bytes());
    let signing_input = format!("{header}.{body}");
    let signature = base64url_encode(&hmac_sha256(secret, signing_input.as_bytes()));
    format!("{signing_input}.{signature}")
}

/// Verify a JWT's signature and claims.
pub fn verify_token(token: &str, secret: &[u8]) -> Result<Verified, String> {
    verify_token_at(token, secret, now_seconds())
}

fn decode_json(part: &str) -> Result<Map<String, Value>, String> {
    let bytes = base64url_decode(part).ok_or("Invalid Compact JWS")?;
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err("Invalid Compact JWS".into()),
    }
}

fn verify_token_at(token: &str, secret: &[u8], now: i64) -> Result<Verified, String> {
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return Err("Invalid Compact JWS".into());
    };
    let header_json = decode_json(header)?;
    if header_json.get("alg").and_then(Value::as_str) != Some("HS256") {
        return Err("\"alg\" (Algorithm) Header Parameter value not allowed".into());
    }
    if header_json.contains_key("crit") {
        return Err("Unsupported \"crit\" Header Parameter".into());
    }
    let signature = base64url_decode(signature).ok_or("Invalid Compact JWS")?;
    if !hmac_sha256_verify(secret, format!("{header}.{payload}").as_bytes(), &signature) {
        return Err("signature verification failed".into());
    }
    let claims = decode_json(payload)?;
    if claims.get("iss").and_then(Value::as_str) != Some(JWT_ISSUER) {
        return Err("unexpected \"iss\" claim value".into());
    }
    for key in ["iat", "nbf", "exp"] {
        if claims.get(key).is_some_and(|v| !v.is_number()) {
            return Err(format!("\"{key}\" claim must be a number"));
        }
    }
    if claims
        .get("nbf")
        .and_then(Value::as_f64)
        .is_some_and(|nbf| nbf > now as f64)
    {
        return Err("\"nbf\" claim timestamp check failed".into());
    }
    let exp = claims.get("exp").and_then(Value::as_f64);
    if exp.is_some_and(|exp| exp <= now as f64) {
        return Err("\"exp\" claim timestamp check failed".into());
    }
    let token_type = match claims.get("typ").and_then(Value::as_str) {
        Some("access") => TokenType::Access,
        Some("refresh") => TokenType::Refresh,
        _ => return Err("Invalid token type".into()),
    };
    let client_id = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("Token missing subject (sub)")?;
    let scopes = match claims.get("scope").and_then(Value::as_str) {
        Some(scope) if !scope.is_empty() => scope.split(' ').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    let expires_at = exp.ok_or("Token missing expiration (exp)")? as i64;
    Ok(Verified {
        token_type,
        client_id: client_id.to_string(),
        scopes,
        resource: claims
            .get("resource")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_at,
        jti: claims
            .get("jti")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(token_type: TokenType) -> SignOptions {
        SignOptions {
            client_id: "client".into(),
            scopes: vec!["mcp".into()],
            resource: Some("https://example.com/".into()),
            ttl_seconds: 60,
            token_type,
            jti: Some("j1".into()),
        }
    }

    #[test]
    fn round_trips_claims() {
        let token = sign_token_at(&options(TokenType::Refresh), b"secret", 1000);
        let payload = decode_json(token.split('.').nth(1).unwrap()).unwrap();
        let keys: Vec<&str> = payload.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "typ", "sub", "scope", "resource", "jti", "iss", "iat", "exp"
            ]
        );
        let verified = verify_token_at(&token, b"secret", 1010).unwrap();
        assert_eq!(verified.token_type, TokenType::Refresh);
        assert_eq!(verified.client_id, "client");
        assert_eq!(verified.scopes, vec!["mcp".to_string()]);
        assert_eq!(verified.expires_at, 1060);
        assert_eq!(verified.jti.as_deref(), Some("j1"));
    }

    #[test]
    fn rejects_tampering_expiry_and_wrong_keys() {
        let token = sign_token_at(&options(TokenType::Access), b"secret", 1000);
        assert!(verify_token_at(&token, b"other", 1010).is_err());
        assert!(verify_token_at(&token, b"secret", 1060).is_err());
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        parts[1] =
            base64url_encode(br#"{"typ":"access","sub":"evil","iss":"whoop-mcp","exp":99999}"#);
        assert!(verify_token_at(&parts.join("."), b"secret", 1010).is_err());
        let none_alg = format!("{}.{}.", base64url_encode(br#"{"alg":"none"}"#), parts[1]);
        assert!(verify_token_at(&none_alg, b"secret", 1010).is_err());
        assert!(verify_token_at("a.b", b"secret", 1010).is_err());
    }
}
