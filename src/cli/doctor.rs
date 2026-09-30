//! `whoop-mcp doctor`: local-only configuration diagnostics (no network requests).

use crate::auth::token_store::home_dir;
use serde_json::{Value, json};

/// Token file inspection result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenInspection {
    pub regular: bool,
    pub mode: u32,
    pub directory_mode: u32,
}

/// Inputs for [`run_doctor`] (injectable for tests).
pub struct DoctorDependencies<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub windows: bool,
    pub inspect_token: &'a dyn Fn() -> std::io::Result<TokenInspection>,
    pub write: &'a mut dyn FnMut(&str),
}

/// Inspect `~/.whoop-mcp/tokens.json` without following symlinks.
pub fn inspect_token() -> std::io::Result<TokenInspection> {
    let directory = home_dir().join(".whoop-mcp");
    let folder = std::fs::symlink_metadata(&directory)?;
    let file = std::fs::symlink_metadata(directory.join("tokens.json"))?;
    #[cfg(unix)]
    let (mode, directory_mode) = {
        use std::os::unix::fs::PermissionsExt;
        (
            file.permissions().mode() & 0o777,
            folder.permissions().mode() & 0o777,
        )
    };
    #[cfg(not(unix))]
    let (mode, directory_mode) = (0, 0);
    Ok(TokenInspection {
        regular: folder.is_dir()
            && !folder.file_type().is_symlink()
            && file.is_file()
            && !file.file_type().is_symlink(),
        mode,
        directory_mode,
    })
}

/// `Number(raw)` for port strings.
fn js_number(raw: &str) -> f64 {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).map_or(f64::NAN, |v| v as f64);
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return trimmed.parse().unwrap_or(f64::NAN);
    }
    f64::NAN
}

/// Run diagnostics; returns the process exit code.
pub fn run_doctor(args: &[String], deps: DoctorDependencies<'_>) -> i32 {
    if args.len() > 1 || args.iter().any(|a| a != "--json") {
        (deps.write)("Usage: whoop-mcp doctor [--json]");
        return 2;
    }
    let env = deps.env;
    let transport = env("MCP_TRANSPORT")
        .unwrap_or_else(|| "stdio".into())
        .trim()
        .to_lowercase();
    let privacy = env("WHOOP_MCP_PRIVACY_MODE").unwrap_or_else(|| "standard".into());
    let privacy_ok = privacy == "standard" || privacy == "aggregate";
    let port = js_number(&env("MCP_PORT").unwrap_or_else(|| "3000".into()));
    let filled = |key: &str| env(key).is_some_and(|v| !v.trim().is_empty());
    let transport_known = ["stdio", "http", "both"].contains(&transport.as_str());
    let token_ok = match (deps.inspect_token)() {
        Ok(token) => {
            !deps.windows && token.regular && token.mode == 0o600 && token.directory_mode == 0o700
        }
        Err(_) => false,
    };
    let checks = [
        ("runtime", true),
        (
            "credentials_configured",
            filled("WHOOP_CLIENT_ID") && filled("WHOOP_CLIENT_SECRET"),
        ),
        (
            "transport_configuration",
            transport_known
                && (transport == "stdio"
                    || (filled("MCP_AUTH_TOKEN")
                        && port.fract() == 0.0
                        && (0.0..=65535.0).contains(&port))),
        ),
        ("privacy_configuration", privacy_ok),
        ("private_token_file", token_ok),
    ];
    let ready = checks.iter().all(|(_, ok)| *ok);
    let next_step = "Use setup --verify for explicit live authorization verification. No network requests were made.";
    if args.iter().any(|a| a == "--json") {
        let mut check_map = serde_json::Map::new();
        for (name, ok) in checks {
            check_map.insert(name.into(), Value::from(ok));
        }
        let report = json!({
            "ready": ready,
            "checks": check_map,
            "transport": if transport_known { transport.as_str() } else { "invalid" },
            "privacy_mode": if privacy_ok { privacy.as_str() } else { "invalid" },
            "scope_status": "unknown",
            "token_validity": "unknown",
            "next_step": next_step
        });
        (deps.write)(&crate::js::stringify_pretty(&report));
    } else {
        let mut lines = vec![
            if ready {
                "Local checks passed."
            } else {
                "Local configuration needs attention."
            }
            .to_string(),
        ];
        lines.extend(
            checks.iter().map(|(name, ok)| {
                format!("{name}: {}", if *ok { "pass" } else { "needs attention" })
            }),
        );
        lines.push(next_step.into());
        (deps.write)(&lines.join("\n"));
    }
    i32::from(!ready)
}

/// Run doctor against the real environment, printing to stdout.
pub fn run_doctor_default(args: &[String]) -> i32 {
    let env = |key: &str| std::env::var(key).ok();
    let mut write = |text: &str| println!("{text}");
    run_doctor(
        args,
        DoctorDependencies {
            env: &env,
            windows: cfg!(windows),
            inspect_token: &inspect_token,
            write: &mut write,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(
        args: &[&str],
        vars: &[(&str, &str)],
        token: std::io::Result<TokenInspection>,
    ) -> (i32, String) {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        let env = move |key: &str| vars.get(key).cloned();
        let token = std::sync::Mutex::new(Some(token));
        let inspect = || token.lock().unwrap().take().unwrap();
        let mut output = String::new();
        let mut write = |text: &str| output.push_str(text);
        let args: Vec<String> = args.iter().map(|a| (*a).into()).collect();
        let code = run_doctor(
            &args,
            DoctorDependencies {
                env: &env,
                windows: false,
                inspect_token: &inspect,
                write: &mut write,
            },
        );
        (code, output)
    }

    fn private() -> std::io::Result<TokenInspection> {
        Ok(TokenInspection {
            regular: true,
            mode: 0o600,
            directory_mode: 0o700,
        })
    }

    #[test]
    fn reports_ready_configuration() {
        let (code, output) = run(
            &[],
            &[("WHOOP_CLIENT_ID", "a"), ("WHOOP_CLIENT_SECRET", "b")],
            private(),
        );
        assert_eq!(code, 0);
        assert!(
            output.starts_with("Local checks passed.\nruntime: pass\ncredentials_configured: pass")
        );
    }

    #[test]
    fn flags_problems_in_json() {
        let (code, output) = run(
            &["--json"],
            &[("MCP_TRANSPORT", "http"), ("WHOOP_MCP_PRIVACY_MODE", "x")],
            Err(std::io::ErrorKind::NotFound.into()),
        );
        assert_eq!(code, 1);
        let report: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(report["ready"], false);
        assert_eq!(report["checks"]["transport_configuration"], false);
        assert_eq!(report["privacy_mode"], "invalid");
        assert_eq!(report["transport"], "http");
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert_eq!(run(&["--bogus"], &[], private()).0, 2);
    }
}
