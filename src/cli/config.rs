//! MCP client configuration generators (pure functions).

use serde_json::{Map, Value, json};
use std::path::PathBuf;

/// Supported client targets.
pub const CLIENT_TARGETS: [&str; 13] = [
    "claude-desktop",
    "claude-code",
    "codex",
    "copilot",
    "claude",
    "claude-web",
    "chatgpt",
    "grok",
    "perplexity",
    "gemini-cli",
    "cursor",
    "vscode",
    "openclaw",
];

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

/// Configuration fragments for clients whose settings are merged by the user.
pub fn generate_local_setup_instructions(
    target: &str,
    env: &ServerEnv,
    command: &str,
) -> Option<String> {
    let entry = generate_claude_desktop_entry(env, command);
    let warning = "Contains credentials: keep this private and out of version control.\n";
    match target {
        "gemini-cli" | "cursor" => {
            let (path, source) = if target == "cursor" {
                ("~/.cursor/mcp.json", "https://cursor.com/docs/context/mcp")
            } else {
                (
                    "~/.gemini/settings.json",
                    "https://geminicli.com/docs/tools/mcp-server/",
                )
            };
            let config = json!({"mcpServers": {"whoop": entry}});
            Some(format!(
                "Merge this into {path}, preserving other settings and servers:\n{warning}\n{config:#}\n\nRestart the client and approve the WHOOP server. Keep the binary at the configured absolute path.\n{source}\n"
            ))
        }
        "openclaw" => {
            let config = json!({"mcp": {"servers": {"whoop": {
                "transport": "stdio", "command": command, "args": [], "env": entry_env(env),
            }}}});
            Some(format!(
                "In OpenClaw Settings > MCP, merge this into the scoped config editor, preserving other settings and servers:\n{warning}\n{config:#}\n\nUse a current OpenClaw version with native mcp.servers support (not the separate mcporter registry).\nThe server runs on the Gateway host; the binary and WHOOP authorization must be available there.\nVerify after saving: openclaw mcp doctor whoop --probe\nhttps://docs.openclaw.ai/tools/mcp\n"
            ))
        }
        "perplexity" => {
            let mut vars = format!(
                "WHOOP_CLIENT_ID={} WHOOP_CLIENT_SECRET={}",
                credential(env, "WHOOP_CLIENT_ID"),
                credential(env, "WHOOP_CLIENT_SECRET"),
            );
            for (key, value) in telemetry_settings(env) {
                vars.push_str(&format!(" {key}={}", shell_quote(&value)));
            }
            Some(format!(
                "Perplexity local MCP requires the macOS app and PerplexityXPC helper.\nOpen Plugins > Add custom > Simple, name it whoop, and paste this launch command:\n{warning}\n  env {vars} {}\n\nSave, wait for Running, then enable it under Sources. Keep the binary at the configured absolute path.\nFor web/non-macOS clients, use Perplexity's remote connector workflow and an authenticated HTTPS deployment instead; availability depends on your account.\nhttps://www.perplexity.ai/help-center/en/articles/11502712-local-and-remote-mcps-for-perplexity\n",
                shell_quote(command)
            ))
        }
        _ => None,
    }
}

/// Remote guidance never consumes WHOOP credentials or performs I/O.
pub fn remote_setup_instructions(target: &str) -> Option<String> {
    let instructions = match target {
        "claude-web" => "Claude web/mobile: in Settings > Connectors, add a custom connector using your public /mcp URL. Configure the existing OAuth connector on the host and enter its connector password on the host's authorization page.\nhttps://support.claude.com/en/articles/11175166-get-started-with-custom-connectors-using-remote-mcp".to_string(),
        "chatgpt" => "ChatGPT: use a custom MCP app/connector in an account/workspace with developer mode enabled. Supply your public /mcp URL and configure OAuth. Availability and admin approval depend on your plan/workspace. ChatGPT is not the Codex CLI.\nhttps://help.openai.com/en/articles/12584461-connecting-custom-mcp-servers-in-chatgpt".to_string(),
        "grok" => {
            let tool = json!({
                "type": "mcp",
                "server_label": "whoop",
                "server_url": "https://YOUR_HOST/mcp",
                "authorization": "YOUR_MCP_AUTH_TOKEN",
            });
            format!("Grok support here means the xAI API remote MCP tool, not a verified custom-connector feature in the consumer Grok chat app. Add this tool to your API request (replace both placeholders; never use a WHOOP token):\n{tool:#}\nThe xAI API credential is separate from MCP_AUTH_TOKEN.\nhttps://docs.x.ai/developers/tools/remote-mcp")
        }
        _ => return None,
    };
    Some(format!(
        "Remote setup guidance only: not deployed or verified.\n\n{instructions}\n\n\
        Deploy either implementation with MCP_TRANSPORT=http behind public HTTPS; the MCP URL is https://YOUR_HOST/mcp (replace YOUR_HOST). Local stdio and localhost are not reachable by cloud clients.\n\
        Keep WHOOP credentials and private tokens on the server host. Bootstrap WHOOP authorization there before connecting; this is a single-account service, not a multi-user relay.\n\
        Keep MCP_AUTH_TOKEN authentication enabled. For OAuth clients, also configure PUBLIC_URL, MCP_CONNECTOR_PASSWORD and exact ALLOWED_REDIRECT_URIS. The existing connector uses one static MCP_OAUTH_CLIENT_ID (default whoop-mcp-connector); configure that client ID in the app where supported. Clients requiring dynamic registration or incompatible OAuth flows may need server changes, outside this setup command.\n\
        Do not disable authentication to bypass incompatibilities. Setup does not provision hosting, change OAuth, or test the client connection.\n\
        Deployment prerequisites: https://github.com/shashankswe2020-ux/whoop-mcp/blob/main/references/deployment.md\n"
    ))
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
    fn additional_local_configs_have_documented_shapes() {
        for target in ["gemini-cli", "cursor", "openclaw"] {
            let text =
                generate_local_setup_instructions(target, &env(), "/path with spaces/whoop-mcp")
                    .unwrap();
            let config: Value =
                serde_json::from_str(&text[text.find('{').unwrap()..=text.rfind('}').unwrap()])
                    .unwrap();
            let entry = generate_claude_desktop_entry(&env(), "/path with spaces/whoop-mcp");
            if target == "openclaw" {
                assert_eq!(config["mcp"]["servers"]["whoop"]["transport"], "stdio");
                assert_eq!(
                    config["mcp"]["servers"]["whoop"]["command"],
                    entry["command"]
                );
                assert_eq!(config["mcp"]["servers"]["whoop"]["args"], json!([]));
                assert_eq!(config["mcp"]["servers"]["whoop"]["env"], entry["env"]);
            } else {
                assert_eq!(config, json!({"mcpServers": {"whoop": entry}}));
            }
        }
    }

    #[test]
    fn perplexity_command_quotes_credentials_and_binary_path() {
        let text = generate_local_setup_instructions(
            "perplexity",
            &env(),
            "/path with 'quotes'/whoop-mcp",
        )
        .unwrap();
        assert!(text.contains("macOS"));
        assert!(text.contains("env WHOOP_CLIENT_ID='id'"));
        assert!(text.contains("WHOOP_CLIENT_SECRET='s'\\''cret'"));
        assert!(text.contains("WHOOP_MCP_TELEMETRY='0' '/path with '\\''quotes'\\''/whoop-mcp'"));
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
