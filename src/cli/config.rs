//! MCP client configuration generators (pure functions).

use serde_json::{Map, Value, json};
use std::path::PathBuf;

/// Supported client targets.
pub const CLIENT_TARGETS: [&str; 4] = ["claude-desktop", "claude-code", "codex", "copilot"];

const SERVER_NAME: &str = "whoop";

/// Ordered environment for the server entry.
pub type ServerEnv = Map<String, Value>;

/// OS-specific path to `claude_desktop_config.json`.
pub fn claude_desktop_config_path() -> PathBuf {
    let home = crate::auth::token_store::home_dir();
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("Claude")
            .join("claude_desktop_config.json")
    } else if cfg!(windows) {
        let app_data = std::env::var_os("APPDATA")
            .map_or_else(|| home.join("AppData").join("Roaming"), PathBuf::from);
        app_data.join("Claude").join("claude_desktop_config.json")
    } else {
        home.join(".config")
            .join("Claude")
            .join("claude_desktop_config.json")
    }
}

/// Absolute path of the running binary (falls back to `whoop-mcp` on `PATH`).
pub fn server_command() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map_or_else(|| "whoop-mcp".to_string(), |p| p.display().to_string())
}

fn telemetry_settings(env: &ServerEnv) -> Vec<(&'static str, String)> {
    ["WHOOP_MCP_TELEMETRY", "WHOOP_MCP_TELEMETRY_ENDPOINT"]
        .into_iter()
        .filter_map(|key| {
            env.get(key)
                .and_then(Value::as_str)
                .map(|v| (key, v.to_string()))
        })
        .collect()
}

fn entry_env(env: &ServerEnv) -> ServerEnv {
    let mut out = Map::new();
    for key in ["WHOOP_CLIENT_ID", "WHOOP_CLIENT_SECRET"] {
        out.insert(key.into(), env.get(key).cloned().unwrap_or(Value::from("")));
    }
    for (key, value) in telemetry_settings(env) {
        out.insert(key.into(), Value::from(value));
    }
    out
}

/// Claude Desktop `mcpServers.whoop` entry.
pub fn generate_claude_desktop_entry(env: &ServerEnv, command: &str) -> Value {
    json!({"command": command, "args": [], "env": entry_env(env)})
}

/// Merge the whoop entry into an existing Claude Desktop config.
pub fn merge_claude_desktop_config(existing: Option<Value>, entry: Value) -> Value {
    let mut base = match existing {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut servers = match base.get("mcpServers") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    servers.insert(SERVER_NAME.into(), entry);
    base.insert("mcpServers".into(), Value::Object(servers));
    Value::Object(base)
}

/// Single-quote for POSIX shells.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn credential(env: &ServerEnv, key: &str) -> String {
    shell_quote(env.get(key).and_then(Value::as_str).unwrap_or(""))
}

fn telemetry_flags(env: &ServerEnv, flag: &str) -> String {
    telemetry_settings(env)
        .iter()
        .map(|(k, v)| format!(" {flag} {k}={}", shell_quote(v)))
        .collect()
}

/// `claude mcp add ...` for Claude Code.
pub fn generate_claude_code_command(env: &ServerEnv, command: &str) -> String {
    format!(
        "claude mcp add {SERVER_NAME} -e WHOOP_CLIENT_ID={} -e WHOOP_CLIENT_SECRET={}{} -- {}",
        credential(env, "WHOOP_CLIENT_ID"),
        credential(env, "WHOOP_CLIENT_SECRET"),
        telemetry_flags(env, "-e"),
        shell_quote(command)
    )
}

/// `codex mcp add ...` for the OpenAI Codex CLI.
pub fn generate_codex_command(env: &ServerEnv, command: &str) -> String {
    format!(
        "codex mcp add {SERVER_NAME} --env WHOOP_CLIENT_ID={} --env WHOOP_CLIENT_SECRET={}{} -- {}",
        credential(env, "WHOOP_CLIENT_ID"),
        credential(env, "WHOOP_CLIENT_SECRET"),
        telemetry_flags(env, "--env"),
        shell_quote(command)
    )
}

/// `code --add-mcp ...` for GitHub Copilot in VS Code.
pub fn generate_copilot_command(env: &ServerEnv, command: &str) -> String {
    let payload =
        json!({"name": SERVER_NAME, "command": command, "args": [], "env": entry_env(env)});
    format!("code --add-mcp {}", shell_quote(&payload.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> ServerEnv {
        let mut env = Map::new();
        env.insert("WHOOP_CLIENT_ID".into(), Value::from("id"));
        env.insert("WHOOP_CLIENT_SECRET".into(), Value::from("s'cret"));
        env.insert("WHOOP_MCP_TELEMETRY".into(), Value::from("0"));
        env
    }

    #[test]
    fn generates_shell_safe_commands() {
        assert_eq!(
            generate_claude_code_command(&env(), "/bin/whoop-mcp"),
            "claude mcp add whoop -e WHOOP_CLIENT_ID='id' -e WHOOP_CLIENT_SECRET='s'\\''cret' -e WHOOP_MCP_TELEMETRY='0' -- '/bin/whoop-mcp'"
        );
        assert!(
            generate_codex_command(&env(), "w")
                .starts_with("codex mcp add whoop --env WHOOP_CLIENT_ID='id'")
        );
        assert!(
            generate_copilot_command(&env(), "w")
                .starts_with("code --add-mcp '{\"name\":\"whoop\"")
        );
    }

    #[test]
    fn merges_preserving_other_servers() {
        let existing = json!({"theme": "dark", "mcpServers": {"other": {"command": "x"}, "whoop": {"command": "old"}}});
        let merged =
            merge_claude_desktop_config(Some(existing), generate_claude_desktop_entry(&env(), "w"));
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["mcpServers"]["other"]["command"], "x");
        assert_eq!(merged["mcpServers"]["whoop"]["command"], "w");
        let keys: Vec<&String> = merged["mcpServers"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["other", "whoop"]);
    }
}
