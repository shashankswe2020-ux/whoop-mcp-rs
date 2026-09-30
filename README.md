# whoop-mcp (Rust)

[![crates.io](https://img.shields.io/crates/v/whoop-mcp?color=16803c)](https://crates.io/crates/whoop-mcp)
[![docs.rs](https://img.shields.io/docsrs/whoop-mcp)](https://docs.rs/whoop-mcp)
[![MIT license](https://img.shields.io/badge/license-MIT-555)](LICENSE)

A read-only [WHOOP](https://www.whoop.com/) [MCP server](https://modelcontextprotocol.io/)
for Claude Desktop, Claude Code, Codex, and GitHub Copilot, shipped as a single
native binary.

This is the Rust implementation of
[`whoop-ai-mcp`](https://www.npmjs.com/package/whoop-ai-mcp)
([TypeScript source](https://github.com/shashankswe2020-ux/whoop-mcp)). It exposes
the same **16 tools, 4 resources, and 5 prompts** with identical names,
descriptions, input/output schemas, and results, and needs no Node.js runtime
or npm dependency tree.

## Install

```bash
cargo install whoop-mcp
```

Requires Rust 1.88+, an active WHOOP membership, and a
[WHOOP Developer App](https://developer.whoop.com) with the redirect URL
`http://localhost:3000/callback`.

## Quickstart

```bash
whoop-mcp setup --client=claude-desktop
```

Enter your app credentials, fully quit and reopen Claude Desktop, authorize WHOOP
in your browser on the first request, and ask:

> **"Summarize my sleep and recovery this week."**

For other clients use `--client=claude-code`, `--client=codex`, or
`--client=copilot` and run the command that setup prints. Add `--verify` to run
the OAuth flow and fetch your profile immediately. Setup registers the absolute
path of the installed binary, so GUI clients do not depend on your shell `PATH`.

Manual Claude Desktop configuration:

```json
{
  "mcpServers": {
    "whoop": {
      "command": "/Users/you/.cargo/bin/whoop-mcp",
      "args": [],
      "env": { "WHOOP_CLIENT_ID": "...", "WHOOP_CLIENT_SECRET": "..." }
    }
  }
}
```

## Commands

| Command | Purpose |
|---------|---------|
| `whoop-mcp` | Start the server (`MCP_TRANSPORT=stdio` by default) |
| `whoop-mcp setup [--client …] [--client-id …] [--client-secret …] [--verify] [--telemetry on\|off]` | Configure credentials and an MCP client |
| `whoop-mcp doctor [--json]` | Local-only configuration checks (no network requests) |
| `whoop-mcp telemetry status` | Show telemetry consent status |
| `whoop-mcp --version` / `--help` | Version and usage |

## Tools, resources, and prompts

| Tools | |
|-------|---|
| Raw data | `get_profile`, `get_body_measurement`, `get_recovery_collection`, `get_sleep_collection`, `get_workout_collection`, `get_cycle_collection`, `get_sleep_by_id`, `get_workout_by_id`, `get_cycle_by_id` |
| Summaries | `get_today`, `get_weekly_summary`, `get_calendar`, `compare_periods`, `get_trend` |
| Personal analytics | `get_baselines`, `get_sleep_debt` |

Collection tools accept ISO 8601 or relative dates (`today`, `last 7 days`,
`this week`, `last month`, `2026-09`, …). Resources:
`whoop://v2/user/{recovery,sleep,cycle}/latest` and `whoop://v2/user/profile`.
Prompts: `weekly_health_review`, `sleep_analysis`, `recovery_trend`,
`workout_recap`, `health_check`. Full parameter reference:
[tools.md](https://github.com/shashankswe2020-ux/whoop-mcp/blob/main/references/tools.md).

Statistics describe your recorded history, not medical diagnoses or predictions.

## Configuration

| Variable | Default | Purpose |
|----------|---------|---------|
| `WHOOP_CLIENT_ID`, `WHOOP_CLIENT_SECRET` | required | WHOOP developer app credentials |
| `WHOOP_REDIRECT_URI` | `http://localhost:3000/callback` | Loopback OAuth callback (`localhost`, `127.0.0.1`, or `[::1]`) |
| `WHOOP_MCP_PRIVACY_MODE` | `standard` | `aggregate` exposes only five summary tools with date-only periods |
| `WHOOP_MCP_DISABLE_RESOURCES` | unset | `1` disables MCP resources |
| `MCP_TRANSPORT` | `stdio` | `stdio`, `http`, or `both` |
| `MCP_AUTH_TOKEN` | — | Bearer token required for HTTP transport |
| `MCP_PORT`, `MCP_HOST` | `3000`, `0.0.0.0` | HTTP listener |
| `MCP_ALLOWED_ORIGINS` | none | Comma-separated CORS allowlist |
| `MCP_TRUST_PROXY` | unset | `1` trusts `X-Forwarded-For` for rate limiting |
| `MCP_CONNECTOR_PASSWORD`, `PUBLIC_URL`, `ALLOWED_REDIRECT_URIS` | unset | Enable the OAuth 2.1 connector for claude.ai web/mobile (all three required; HTTPS public URL; password ≥ 12 characters) |
| `MCP_JWT_SECRET`, `MCP_OAUTH_CLIENT_ID` | derived, `whoop-mcp-connector` | Connector signing key and client ID |
| `LOG_LEVEL`, `LOG_FORMAT` | `info`, `json` | Structured stderr logging |
| `WHOOP_MCP_TELEMETRY`, `WHOOP_MCP_TELEMETRY_ENDPOINT`, `DO_NOT_TRACK` | off | Opt-in aggregate usage telemetry |

Tokens are stored at `~/.whoop-mcp/tokens.json` with `0600` permissions (directory
`0700`) and shared with the npm package, so switching implementations does not
require re-authorizing.

## Privacy and security

- Read-only WHOOP scopes; OAuth Authorization Code flow with PKCE (S256) and state checks.
- Health results reach only the assistant provider you configure. Aggregate mode
  limits disclosure; it is not anonymization.
- Telemetry is off unless you opt in and never includes health data, arguments,
  chat text, or identifiers. `DO_NOT_TRACK=1` and aggregate mode always disable it.
  Rust builds report their version with a `-rust` suffix.
- HTTP transport: constant-time bearer comparison, per-IP rate limiting,
  connection limits, CORS allowlist, header read timeouts, 1 MB body limit.
- Minimal dependencies: `tokio`, `hyper`, `reqwest` (rustls with the `ring`
  provider and platform certificate verification), `serde_json`, `url`, `chrono`,
  `ring`. No OpenSSL.

## Compatibility with `whoop-ai-mcp`

Behavior is verified against the TypeScript implementation:

- `tools/list`, prompts, resources, and protocol error responses are byte-identical
  (tool definitions are embedded from the TypeScript build in `assets/`).
- `tests/parity.rs` replays 64 tool/resource scenarios (standard and aggregate
  privacy, API errors, outages, empty data) generated by
  `scripts/generate-parity-fixtures.mjs` and requires identical results.
- `scripts/compare-http.sh` diffs 62 HTTP transport and OAuth connector requests
  against a running TypeScript server.

Intentional differences:

- The binary is `whoop-mcp`; setup registers its absolute path instead of `npx`.
- The HTTP transport supports up to 64 concurrent MCP sessions (the TypeScript
  transport accepts one session per process).
- `--help` and `--version` are available; the doctor `runtime` check always passes.

## Development

```bash
cargo test                        # unit, integration, and parity tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# Regenerate parity fixtures / compare HTTP transcripts (needs a built TypeScript checkout)
TZ=UTC node scripts/generate-parity-fixtures.mjs ../whoop-mcp
scripts/compare-http.sh ../whoop-mcp
```

## License

MIT — see [LICENSE](LICENSE). Not affiliated with or endorsed by WHOOP, Inc.
