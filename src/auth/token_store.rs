//! File-based OAuth token storage at `~/.whoop-mcp/tokens.json` (0600).

use crate::logging::now_ms;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TOKEN_FILENAME: &str = "tokens.json";
/// Tokens within this window of expiry are treated as expired.
const EXPIRY_BUFFER_MS: f64 = 60_000.0;

/// Stored OAuth token set with absolute expiry time.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch milliseconds.
    pub expires_at: f64,
    pub token_type: String,
}

impl OAuthTokens {
    /// Serialize like `JSON.stringify(tokens, null, 2)`.
    pub fn to_json_string(&self) -> String {
        crate::js::stringify_pretty(&json!({
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "expires_at": crate::js::num(self.expires_at),
            "token_type": self.token_type,
        }))
    }
}

/// The user's home directory (`os.homedir()`).
pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Configured or default (`~/.whoop-mcp`) directory for OAuth state files.
pub fn resolve_token_directory(token_dir: Option<&Path>) -> PathBuf {
    token_dir.map_or_else(|| home_dir().join(".whoop-mcp"), Path::to_path_buf)
}

fn token_file_path(token_dir: Option<&Path>) -> PathBuf {
    resolve_token_directory(token_dir).join(TOKEN_FILENAME)
}

/// Replace the home directory prefix with `~` for logging.
pub fn redact_home_path(path: &Path) -> String {
    let text = path.display().to_string();
    let home = home_dir().display().to_string();
    match text.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => text,
    }
}

/// Whether tokens are expired or within the 60s safety buffer.
pub fn is_token_expired(tokens: &OAuthTokens) -> bool {
    tokens.expires_at <= now_ms() + EXPIRY_BUFFER_MS
}

/// Create a directory (recursively) with 0700 permissions for new directories.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Write a file, creating it with 0600 permissions (existing files keep their mode).
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)
}

/// Persist tokens to disk.
pub fn save_tokens(tokens: &OAuthTokens, token_dir: Option<&Path>) -> std::io::Result<()> {
    create_private_dir(&resolve_token_directory(token_dir))?;
    write_private_file(
        &token_file_path(token_dir),
        tokens.to_json_string().as_bytes(),
    )
}

fn parse_tokens(value: &Value) -> Option<OAuthTokens> {
    let access_token = value
        .get("access_token")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    let refresh_token = value
        .get("refresh_token")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    let expires_at = value.get("expires_at")?.as_f64()?;
    Some(OAuthTokens {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at,
        token_type: value
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Load tokens; `None` when missing, malformed, or invalid (reason logged to stderr).
pub fn load_tokens(token_dir: Option<&Path>) -> Option<OAuthTokens> {
    let path = token_file_path(token_dir);
    let safe_path = redact_home_path(&path);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => {
                let tokens = parse_tokens(&value);
                if tokens.is_none() {
                    eprintln!("Token file {safe_path} exists but has invalid shape — ignoring.");
                }
                tokens
            }
            Err(error) => {
                eprintln!("Failed to read token file at {safe_path}: {error}");
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("No token file found at {safe_path}.");
            None
        }
        Err(error) => {
            eprintln!("Failed to read token file at {safe_path}: {error}");
            None
        }
    }
}

/// Delete the token file (no-op when missing).
pub fn delete_tokens(token_dir: Option<&Path>) -> std::io::Result<()> {
    match std::fs::remove_file(token_file_path(token_dir)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "whoop-mcp-test-{name}-{}",
            crate::crypto::uuid_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saves_with_private_permissions_and_round_trips() {
        let dir = temp_dir("store").join("nested");
        let tokens = OAuthTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1_700_000_000_000.0,
            token_type: "Bearer".into(),
        };
        save_tokens(&tokens, Some(&dir)).unwrap();
        assert_eq!(load_tokens(Some(&dir)), Some(tokens.clone()));
        let raw = std::fs::read_to_string(dir.join(TOKEN_FILENAME)).unwrap();
        assert!(raw.starts_with("{\n  \"access_token\": \"a\",\n  \"refresh_token\": \"r\",\n  \"expires_at\": 1700000000000,"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(TOKEN_FILENAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        assert!(is_token_expired(&tokens));
        delete_tokens(Some(&dir)).unwrap();
        delete_tokens(Some(&dir)).unwrap();
        assert_eq!(load_tokens(Some(&dir)), None);
    }

    #[test]
    fn rejects_invalid_shapes() {
        let dir = temp_dir("shape");
        std::fs::write(
            dir.join(TOKEN_FILENAME),
            r#"{"access_token":"","refresh_token":"r","expires_at":1}"#,
        )
        .unwrap();
        assert_eq!(load_tokens(Some(&dir)), None);
        std::fs::write(dir.join(TOKEN_FILENAME), "not json").unwrap();
        assert_eq!(load_tokens(Some(&dir)), None);
    }

    #[test]
    fn expiry_uses_safety_buffer() {
        let fresh = OAuthTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_ms() + 120_000.0,
            token_type: String::new(),
        };
        assert!(!is_token_expired(&fresh));
        let soon = OAuthTokens {
            expires_at: now_ms() + 30_000.0,
            ..fresh
        };
        assert!(is_token_expired(&soon));
    }
}
