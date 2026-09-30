//! Interactive `whoop-mcp setup` wizard: credentials, optional live
//! verification, telemetry consent, and client configuration.

use super::config::{
    CLIENT_TARGETS, ServerEnv, claude_desktop_config_path, generate_claude_code_command,
    generate_claude_desktop_entry, generate_codex_command, generate_copilot_command,
    merge_claude_desktop_config, server_command,
};
use crate::api::BoxFuture;
use crate::auth::oauth::{OAuthConfig, authenticate, refresh_access_token, to_oauth_tokens};
use crate::auth::token_store::{load_tokens, save_tokens, write_private_file};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const DEFAULT_TELEMETRY_ENDPOINT: &str =
    "https://whoop-mcp-telemetry.whoop-ai-mcp.workers.dev/events";

/// Parsed `setup` flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupOptions {
    pub telemetry: Option<bool>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub client: Option<String>,
    pub verify: bool,
    pub config_path: Option<PathBuf>,
}

/// Parse `setup` arguments (`--flag value` or `--flag=value`).
pub fn parse_setup_args(argv: &[String]) -> Result<SetupOptions, String> {
    let mut out = SetupOptions::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let (key, inline) = if arg.starts_with("--") {
            match arg.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (arg.clone(), None),
            }
        } else {
            (String::new(), None)
        };
        let mut value = || -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            match argv.get(i + 1) {
                Some(next) if !next.starts_with("--") => {
                    i += 1;
                    Ok(next.clone())
                }
                _ => Err(format!("Missing value for flag {key}")),
            }
        };
        match key.as_str() {
            "--telemetry" => match value()?.as_str() {
                "on" => out.telemetry = Some(true),
                "off" => out.telemetry = Some(false),
                _ => return Err("--telemetry must be on or off".into()),
            },
            "--client-id" => out.client_id = Some(value()?),
            "--client-secret" => out.client_secret = Some(value()?),
            "--client" => {
                let v = value()?;
                if !CLIENT_TARGETS.contains(&v.as_str()) {
                    return Err(format!(
                        "Invalid --client value: \"{v}\". Must be \"claude-desktop\", \"claude-code\", \"codex\", or \"copilot\"."
                    ));
                }
                out.client = Some(v);
            }
            "--verify" => out.verify = true,
            "--config-path" => out.config_path = Some(PathBuf::from(value()?)),
            "" => {}
            other => return Err(format!("Unknown flag: {other}")),
        }
        i += 1;
    }
    Ok(out)
}

/// Terminal interaction used by the wizard.
pub trait Prompter {
    fn write(&mut self, text: &str);
    /// Ask a question and return the trimmed answer (empty on EOF).
    fn prompt(&mut self, question: &str) -> String;
    /// Ask for a secret without echo.
    fn prompt_secret(&mut self, question: &str) -> Result<String, String>;
    fn is_tty(&self) -> bool;
    /// Ask the telemetry consent question.
    fn confirm_telemetry(&mut self) -> bool {
        let answer = self.prompt("Share this usage metadata? [y/N]: ");
        matches!(answer.to_lowercase().as_str(), "y" | "yes")
    }
}

/// Injectable side effects for [`run_setup`].
pub struct SetupDeps<'a> {
    pub prompter: &'a mut dyn Prompter,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub authenticate: &'a dyn Fn(OAuthConfig) -> BoxFuture<'static, Result<String, String>>,
    pub fetch_profile: &'a dyn Fn(String, OAuthConfig) -> BoxFuture<'static, Result<Value, String>>,
    pub command: String,
}

fn env_string(env: &ServerEnv, key: &str) -> Option<String> {
    env.get(key).and_then(Value::as_str).map(str::to_string)
}

fn read_existing_env(path: &Path) -> Option<ServerEnv> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed
        .pointer("/mcpServers/whoop/env")
        .and_then(Value::as_object)
        .cloned()
}

fn read_existing_creds(path: &Path) -> Option<(String, String)> {
    let env = read_existing_env(path)?;
    let id = env_string(&env, "WHOOP_CLIENT_ID")?.trim().to_string();
    let secret = env_string(&env, "WHOOP_CLIENT_SECRET")?.trim().to_string();
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

fn oauth_config(client_id: &str, client_secret: &str) -> OAuthConfig {
    OAuthConfig {
        client_id: client_id.into(),
        client_secret: client_secret.into(),
        ..OAuthConfig::default()
    }
}

async fn verify_credentials(
    deps: &mut SetupDeps<'_>,
    client_id: &str,
    client_secret: &str,
) -> Result<(), String> {
    deps.prompter
        .write("\nVerifying credentials with WHOOP...\n");
    let config = oauth_config(client_id, client_secret);
    let token = (deps.authenticate)(config.clone())
        .await
        .map_err(|e| format!("Verification failed during OAuth: {e}"))?;
    deps.prompter
        .write("OAuth flow complete. Fetching profile...\n");
    let profile = (deps.fetch_profile)(token, config)
        .await
        .map_err(|e| format!("Verification failed fetching profile: {e}"))?;
    deps.prompter.write(&format!(
        "Profile OK: {}\n\n",
        crate::js::stringify(&profile)
    ));
    Ok(())
}

fn choose_telemetry(
    options: &SetupOptions,
    deps: &mut SetupDeps<'_>,
    existing: Option<&ServerEnv>,
) -> ServerEnv {
    let env = deps.env;
    let saved = |key: &str| existing.and_then(|e| env_string(e, key));
    let off = || {
        let mut map = Map::new();
        map.insert("WHOOP_MCP_TELEMETRY".into(), Value::from("0"));
        map
    };
    let endpoint = saved("WHOOP_MCP_TELEMETRY_ENDPOINT")
        .or_else(|| env("WHOOP_MCP_TELEMETRY_ENDPOINT"))
        .unwrap_or_else(|| DEFAULT_TELEMETRY_ENDPOINT.into());
    let env_telemetry = env("WHOOP_MCP_TELEMETRY");
    let suppressed = env("DO_NOT_TRACK").as_deref() == Some("1")
        || saved("DO_NOT_TRACK").as_deref() == Some("1")
        || env("WHOOP_MCP_PRIVACY_MODE")
            .as_deref()
            .unwrap_or("standard")
            != "standard"
        || saved("WHOOP_MCP_PRIVACY_MODE")
            .as_deref()
            .unwrap_or("standard")
            != "standard"
        || env_telemetry.as_deref().is_some_and(|v| v != "1");
    if suppressed || options.telemetry == Some(false) {
        return off();
    }
    if let Some(saved_telemetry) = saved("WHOOP_MCP_TELEMETRY")
        && options.telemetry.is_none()
    {
        let mut map = Map::new();
        map.insert("WHOOP_MCP_TELEMETRY".into(), Value::from(saved_telemetry));
        if let Some(endpoint) = saved("WHOOP_MCP_TELEMETRY_ENDPOINT").filter(|e| !e.is_empty()) {
            map.insert("WHOOP_MCP_TELEMETRY_ENDPOINT".into(), Value::from(endpoint));
        }
        return map;
    }
    let mut enabled = options.telemetry == Some(true) || env_telemetry.as_deref() == Some("1");
    let tty = deps.prompter.is_tty();
    if !enabled && !tty {
        return off();
    }
    let url = url::Url::parse(&endpoint).ok().filter(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && !endpoint.contains('?')
            && !endpoint.contains('#')
    });
    let Some(url) = url else {
        deps.prompter.write("Telemetry remains off: endpoint must be HTTPS without credentials, query, or fragment.\n");
        return off();
    };
    deps.prompter.write(&format!(
        "\nOptional usage telemetry to {url}: command/tool/prompt names, outcomes and package version only. No health data, chat text, arguments or identifiers. Cloudflare receives network metadata. Disable with WHOOP_MCP_TELEMETRY=0 or DO_NOT_TRACK=1.\n"
    ));
    if !enabled && tty {
        enabled = deps.prompter.confirm_telemetry();
    }
    if !enabled {
        return off();
    }
    let mut map = Map::new();
    map.insert("WHOOP_MCP_TELEMETRY".into(), Value::from("1"));
    map.insert(
        "WHOOP_MCP_TELEMETRY_ENDPOINT".into(),
        Value::from(url.to_string()),
    );
    map
}

fn write_claude_desktop_config(
    path: &Path,
    env: &ServerEnv,
    command: &str,
    prompter: &mut dyn Prompter,
    preserve_entry: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let (existing, existing_raw) = match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => (Some(value), Some(raw)),
            Err(error) => {
                return Err(format!(
                    "Could not parse existing Claude Desktop config at {}: {error}. Refusing to overwrite — fix or move the file and re-run setup.",
                    path.display()
                ));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(error) => {
            return Err(format!(
                "Could not parse existing Claude Desktop config at {}: {error}. Refusing to overwrite — fix or move the file and re-run setup.",
                path.display()
            ));
        }
    };

    let original = existing
        .as_ref()
        .and_then(|e| e.pointer("/mcpServers/whoop"))
        .cloned();
    let merge_env = |base: Option<&Value>| {
        let mut merged = base.and_then(Value::as_object).cloned().unwrap_or_default();
        for (key, value) in env {
            merged.insert(key.clone(), value.clone());
        }
        Value::Object(merged)
    };
    let entry = match original.clone() {
        Some(mut original) if preserve_entry && original.is_object() => {
            let merged = merge_env(original.get("env"));
            original["env"] = merged;
            original
        }
        _ => {
            let mut entry = generate_claude_desktop_entry(env, command);
            entry["env"] = merge_env(original.as_ref().and_then(|o| o.get("env")));
            entry
        }
    };
    let merged = merge_claude_desktop_config(existing, entry);
    let serialized = format!("{}\n", crate::js::stringify_pretty(&merged));

    let backup = PathBuf::from(format!("{}.bak", path.display()));
    let backed_up = match &existing_raw {
        Some(raw) => {
            write_private_file(&backup, raw.as_bytes()).map_err(|e| e.to_string())?;
            true
        }
        None => false,
    };
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let written =
        write_private_file(&tmp, serialized.as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(error) = written {
        if backed_up {
            let _ = std::fs::rename(&backup, path);
        }
        return Err(format!("Failed to write Claude Desktop config: {error}"));
    }
    prompter.write(&format!(
        "\nClaude Desktop config written: {}\n",
        path.display()
    ));
    if backed_up {
        prompter.write(&format!(
            "Previous config backed up to: {}\n",
            backup.display()
        ));
    }
    prompter.write("Restart Claude Desktop to load the new server.\n\n");
    Ok(())
}

/// Run the setup wizard; returns the telemetry consent settings.
pub async fn run_setup(
    options: &SetupOptions,
    deps: &mut SetupDeps<'_>,
) -> Result<ServerEnv, String> {
    deps.prompter
        .write("WHOOP MCP — Setup Wizard\n------------------------\n\n");
    let target = match &options.client {
        Some(client) => client.clone(),
        None => {
            let answer = deps.prompter.prompt(
                "Target client (claude-desktop / claude-code / codex / copilot) [claude-desktop]: ",
            );
            if answer.is_empty() {
                "claude-desktop".into()
            } else {
                answer
            }
        }
    };
    if !CLIENT_TARGETS.contains(&target.as_str()) {
        return Err(format!("Invalid client target: \"{target}\""));
    }
    let desktop_path = options
        .config_path
        .clone()
        .unwrap_or_else(claude_desktop_config_path);
    let explicit = options.client_id.is_some() || options.client_secret.is_some();

    if target == "claude-desktop"
        && !explicit
        && let Some((client_id, client_secret)) = read_existing_creds(&desktop_path)
    {
        deps.prompter.write(&format!(
            "Existing whoop entry found in {}.\n",
            desktop_path.display()
        ));
        if options.verify {
            verify_credentials(deps, &client_id, &client_secret).await?;
            deps.prompter.write("Existing config verified.\n");
        } else {
            deps.prompter
                .write("Re-run with --verify to confirm credentials work.\n");
        }
        let saved = read_existing_env(&desktop_path);
        let consent = choose_telemetry(options, deps, saved.as_ref());
        let saved_telemetry = saved
            .as_ref()
            .and_then(|s| env_string(s, "WHOOP_MCP_TELEMETRY"));
        let consent_telemetry = env_string(&consent, "WHOOP_MCP_TELEMETRY");
        let rewrite = options.telemetry.is_some()
            || (saved_telemetry.as_deref() == Some("1")
                && consent_telemetry.as_deref() == Some("0"))
            || (saved_telemetry.is_none() && deps.prompter.is_tty());
        if rewrite {
            let mut env = Map::new();
            env.insert("WHOOP_CLIENT_ID".into(), Value::from(client_id));
            env.insert("WHOOP_CLIENT_SECRET".into(), Value::from(client_secret));
            env.extend(consent.clone());
            write_claude_desktop_config(&desktop_path, &env, &deps.command, deps.prompter, true)?;
        }
        return Ok(consent);
    }

    let env_id = (deps.env)("WHOOP_CLIENT_ID")
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let env_secret = (deps.env)("WHOOP_CLIENT_SECRET")
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let using_env = !explicit && !env_id.is_empty() && !env_secret.is_empty();
    if using_env {
        deps.prompter.write(
            "Using credentials from environment (WHOOP_CLIENT_ID, WHOOP_CLIENT_SECRET).\n\n",
        );
    }
    let client_id = match &options.client_id {
        Some(id) => id.trim().to_string(),
        None if using_env => env_id,
        None => deps
            .prompter
            .prompt("WHOOP Client ID (from https://developer.whoop.com): ")
            .trim()
            .to_string(),
    };
    if client_id.is_empty() {
        return Err("WHOOP_CLIENT_ID is required".into());
    }
    let client_secret = match &options.client_secret {
        Some(secret) => secret.trim().to_string(),
        None if using_env => env_secret,
        None => deps
            .prompter
            .prompt_secret("WHOOP Client Secret (input hidden): ")?
            .trim()
            .to_string(),
    };
    if client_secret.is_empty() {
        return Err("WHOOP_CLIENT_SECRET is required".into());
    }

    if options.verify {
        verify_credentials(deps, &client_id, &client_secret).await?;
    }

    let saved = if target == "claude-desktop" {
        read_existing_env(&desktop_path)
    } else {
        None
    };
    let consent = choose_telemetry(options, deps, saved.as_ref());
    let mut env = Map::new();
    env.insert("WHOOP_CLIENT_ID".into(), Value::from(client_id));
    env.insert("WHOOP_CLIENT_SECRET".into(), Value::from(client_secret));
    env.extend(consent.clone());

    let command = deps.command.clone();
    match target.as_str() {
        "claude-code" => {
            deps.prompter
                .write("\nRun this command in your shell to register the server:\n\n");
            deps.prompter.write(&format!(
                "  {}\n\n",
                generate_claude_code_command(&env, &command)
            ));
        }
        "codex" => {
            deps.prompter
                .write("\nRun this command in your shell to register the server with Codex:\n\n");
            deps.prompter
                .write(&format!("  {}\n\n", generate_codex_command(&env, &command)));
        }
        "copilot" => {
            deps.prompter.write(
                "\nRun this command to register the server with GitHub Copilot in VS Code:\n\n",
            );
            deps.prompter.write(&format!(
                "  {}\n\n",
                generate_copilot_command(&env, &command)
            ));
        }
        _ => write_claude_desktop_config(&desktop_path, &env, &command, deps.prompter, false)?,
    }
    Ok(consent)
}

// ---------------------------------------------------------------------------
// Real terminal
// ---------------------------------------------------------------------------

/// Prompter over the process stdin/stdout.
pub struct TerminalPrompter;

impl TerminalPrompter {
    fn read_line() -> String {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        line.trim().to_string()
    }
}

impl Prompter for TerminalPrompter {
    fn write(&mut self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    fn prompt(&mut self, question: &str) -> String {
        self.write(question);
        Self::read_line()
    }

    fn prompt_secret(&mut self, question: &str) -> Result<String, String> {
        if !self.is_tty() {
            self.write(&format!("{question}(input will be visible) "));
            return Ok(Self::read_line());
        }
        self.write(question);
        let secret = read_hidden(self);
        self.write("\n");
        secret
    }

    fn is_tty(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }
}

#[cfg(unix)]
fn read_hidden(prompter: &mut TerminalPrompter) -> Result<String, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: termios is plain data; tcgetattr fills it for a valid terminal fd.
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: fd refers to the process stdin, checked to be a terminal.
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return Ok(TerminalPrompter::read_line());
    }
    let mut raw = original;
    raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
    // SAFETY: applying a modified copy of the current settings to the same fd.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    let mut buffer = String::new();
    let mut pending = Vec::new();
    let result = loop {
        let mut byte = [0u8; 1];
        match std::io::stdin().lock().read(&mut byte) {
            Ok(0) | Err(_) => break Ok(buffer.clone()),
            Ok(_) => {}
        }
        match byte[0] {
            b'\n' | b'\r' => break Ok(buffer.clone()),
            3 => break Err("Interrupted".to_string()),
            127 | 8 => {
                if buffer.pop().is_some() {
                    prompter.write("\u{8} \u{8}");
                }
            }
            b if b < 32 => {}
            b => {
                pending.push(b);
                if let Ok(text) = std::str::from_utf8(&pending) {
                    buffer.push_str(text);
                    pending.clear();
                    prompter.write("*");
                } else if pending.len() >= 4 {
                    pending.clear();
                }
            }
        }
    };
    // SAFETY: restore the settings captured above.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    result
}

#[cfg(not(unix))]
fn read_hidden(_: &mut TerminalPrompter) -> Result<String, String> {
    Ok(TerminalPrompter::read_line())
}

/// Default `authenticate` dependency.
pub fn default_authenticate(config: OAuthConfig) -> BoxFuture<'static, Result<String, String>> {
    Box::pin(async move { authenticate(&config).await.map_err(|e| e.to_string()) })
}

/// Default profile fetch with token refresh support.
pub fn default_fetch_profile(
    token: String,
    config: OAuthConfig,
) -> BoxFuture<'static, Result<Value, String>> {
    Box::pin(async move {
        use crate::api::client::{WhoopClient, WhoopClientOptions};
        let refresh_config = config.clone();
        let refresher: crate::api::client::TokenRefresher = std::sync::Arc::new(move || {
            let config = refresh_config.clone();
            Box::pin(async move {
                let tokens = load_tokens(None).ok_or("No stored tokens to refresh")?;
                let refreshed = refresh_access_token(&tokens.refresh_token, &config)
                    .await
                    .map_err(|e| e.to_string())?;
                let fresh = to_oauth_tokens(&refreshed, Some(&tokens.refresh_token));
                save_tokens(&fresh, None).map_err(|e| e.to_string())?;
                Ok(fresh.access_token)
            })
        });
        let client = WhoopClient::new(WhoopClientOptions {
            access_token: token,
            on_token_refresh: Some(refresher),
            ..WhoopClientOptions::default()
        });
        crate::tools::basic::get_profile(&client)
            .await
            .map_err(|e| e.to_string())
    })
}

/// Run setup against the real terminal and environment.
pub async fn run_setup_default(options: &SetupOptions) -> Result<ServerEnv, String> {
    let env = |key: &str| std::env::var(key).ok();
    let mut prompter = TerminalPrompter;
    let mut deps = SetupDeps {
        prompter: &mut prompter,
        env: &env,
        authenticate: &default_authenticate,
        fetch_profile: &default_fetch_profile,
        command: server_command(),
    };
    run_setup(options, &mut deps).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    struct Scripted {
        answers: VecDeque<String>,
        output: String,
        tty: bool,
    }

    impl Prompter for Scripted {
        fn write(&mut self, text: &str) {
            self.output.push_str(text);
        }
        fn prompt(&mut self, question: &str) -> String {
            self.output.push_str(question);
            self.answers.pop_front().unwrap_or_default()
        }
        fn prompt_secret(&mut self, question: &str) -> Result<String, String> {
            Ok(self.prompt(question))
        }
        fn is_tty(&self) -> bool {
            self.tty
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn temp_path() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("whoop-mcp-setup-{}", crate::crypto::uuid_v4()));
        dir.join("claude_desktop_config.json")
    }

    fn never_auth(_: OAuthConfig) -> BoxFuture<'static, Result<String, String>> {
        Box::pin(async { Err("should not authenticate".to_string()) })
    }

    fn fake_profile(_: String, _: OAuthConfig) -> BoxFuture<'static, Result<Value, String>> {
        Box::pin(async { Ok(serde_json::json!({"user_id": 1})) })
    }

    fn ok_auth(_: OAuthConfig) -> BoxFuture<'static, Result<String, String>> {
        Box::pin(async { Ok("token".to_string()) })
    }

    async fn run(
        options: SetupOptions,
        answers: &[&str],
        vars: &[(&str, &str)],
        verify_ok: bool,
    ) -> (Result<ServerEnv, String>, String) {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        let env = move |key: &str| vars.get(key).cloned();
        let mut prompter = Scripted {
            answers: answers.iter().map(|s| (*s).to_string()).collect(),
            output: String::new(),
            tty: false,
        };
        let authenticate: &dyn Fn(OAuthConfig) -> BoxFuture<'static, Result<String, String>> =
            if verify_ok { &ok_auth } else { &never_auth };
        let mut deps = SetupDeps {
            prompter: &mut prompter,
            env: &env,
            authenticate,
            fetch_profile: &fake_profile,
            command: "/usr/local/bin/whoop-mcp".into(),
        };
        let result = run_setup(&options, &mut deps).await;
        (result, prompter.output)
    }

    #[test]
    fn parses_flags() {
        let parsed = parse_setup_args(&args(&[
            "--client=codex",
            "--client-id",
            "abc",
            "--verify",
            "--telemetry=off",
        ]))
        .unwrap();
        assert_eq!(parsed.client.as_deref(), Some("codex"));
        assert_eq!(parsed.client_id.as_deref(), Some("abc"));
        assert!(parsed.verify);
        assert_eq!(parsed.telemetry, Some(false));
        assert_eq!(
            parse_setup_args(&args(&["--client-id"])).unwrap_err(),
            "Missing value for flag --client-id"
        );
        assert_eq!(
            parse_setup_args(&args(&["--nope"])).unwrap_err(),
            "Unknown flag: --nope"
        );
        assert!(
            parse_setup_args(&args(&["--client", "vim"]))
                .unwrap_err()
                .starts_with("Invalid --client value")
        );
    }

    #[tokio::test]
    async fn writes_claude_desktop_config_with_backup() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let options = SetupOptions {
            config_path: Some(path.clone()),
            client: Some("claude-desktop".into()),
            ..SetupOptions::default()
        };
        let (result, output) = run(options, &["my-id", "my-secret"], &[], false).await;
        assert_eq!(result.unwrap()["WHOOP_MCP_TELEMETRY"], "0");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            written["mcpServers"]["whoop"]["command"],
            "/usr/local/bin/whoop-mcp"
        );
        assert_eq!(
            written["mcpServers"]["whoop"]["env"]["WHOOP_CLIENT_ID"],
            "my-id"
        );
        assert!(output.contains("Previous config backed up to:"));
        assert!(PathBuf::from(format!("{}.bak", path.display())).exists());
    }

    #[tokio::test]
    async fn prints_commands_for_cli_clients_and_verifies() {
        let options = SetupOptions {
            client: Some("claude-code".into()),
            client_id: Some("id".into()),
            client_secret: Some("secret".into()),
            verify: true,
            ..SetupOptions::default()
        };
        let (result, output) = run(options, &[], &[], true).await;
        assert!(result.is_ok());
        assert!(output.contains("Profile OK: {\"user_id\":1}"));
        assert!(output.contains("claude mcp add whoop -e WHOOP_CLIENT_ID='id'"));
    }

    #[tokio::test]
    async fn reuses_existing_desktop_entry_and_refuses_corrupt_files() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"mcpServers":{"whoop":{"command":"npx","args":["-y","whoop-ai-mcp"],"env":{"WHOOP_CLIENT_ID":"a","WHOOP_CLIENT_SECRET":"b","WHOOP_MCP_TELEMETRY":"0"}}}}"#).unwrap();
        let options = SetupOptions {
            config_path: Some(path.clone()),
            client: Some("claude-desktop".into()),
            ..SetupOptions::default()
        };
        let (result, output) = run(options, &[], &[], false).await;
        assert_eq!(result.unwrap()["WHOOP_MCP_TELEMETRY"], "0");
        assert!(output.contains("Existing whoop entry found"));

        std::fs::write(&path, "{not json").unwrap();
        let options = SetupOptions {
            config_path: Some(path.clone()),
            client: Some("claude-desktop".into()),
            client_id: Some("a".into()),
            client_secret: Some("b".into()),
            ..SetupOptions::default()
        };
        let (result, _) = run(options, &[], &[], false).await;
        assert!(result.unwrap_err().contains("Refusing to overwrite"));
    }
}
