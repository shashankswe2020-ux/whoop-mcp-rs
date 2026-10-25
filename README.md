# whoop-mcp (Rust)

[![crates.io](https://img.shields.io/crates/v/whoop-mcp?color=16803c)](https://crates.io/crates/whoop-mcp)
[![docs.rs](https://img.shields.io/docsrs/whoop-mcp)](https://docs.rs/whoop-mcp)
[![MIT license](https://img.shields.io/badge/license-MIT-555)](LICENSE)

A read-only [WHOOP](https://www.whoop.com/) [MCP server](https://modelcontextprotocol.io/)
for Claude, Gemini CLI, Cursor, VS Code, OpenClaw, Perplexity on macOS, and other
MCP clients, shipped as a single native binary. ChatGPT, Claude web/mobile, and
the Grok xAI API require a remote deployment (see [client setup](#client-setup)).

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

## Client setup

**Unreleased:** the additional targets below are available in source. Until the
next crate release, use `cargo run -- setup --client=<target>` in this checkout.
After publication, use `whoop-mcp setup --client=<target>`.

| Assistant | Target | What setup does |
|---|---|---|
| Claude Desktop | `claude-desktop` or `claude` | Writes/merges the existing Desktop config with backup |
| Claude Code | `claude-code` | Prints a registration command |
| ChatGPT | `chatgpt` | Prints remote HTTPS + OAuth guidance, not a local launch config |
| Claude web/mobile | `claude-web` | Prints remote HTTPS custom connector guidance |
| Grok | `grok` | Prints an **xAI API** remote MCP tool example; consumer Grok chat is not verified |
| Perplexity | `perplexity` | Prints a local launch command for the **macOS app + PerplexityXPC helper** |
| Gemini CLI | `gemini-cli` | Prints `mcpServers.whoop` for `~/.gemini/settings.json` |
| Cursor | `cursor` | Prints `mcpServers.whoop` for `~/.cursor/mcp.json` |
| VS Code / Copilot | `vscode` or `copilot` | Prints `code --add-mcp` |
| OpenClaw | `openclaw` | Prints native `mcp.servers.whoop` for Settings > MCP |
| Codex | `codex` | Prints a registration command; distinct from ChatGPT |

For example, after installing a release with these targets:

```bash
whoop-mcp setup --client=gemini-cli
whoop-mcp setup --client=cursor
whoop-mcp setup --client=vscode
whoop-mcp setup --client=openclaw
whoop-mcp setup --client=perplexity
```

Merge printed JSON without replacing other settings or servers. Setup only writes
Claude Desktop's config; it does not automatically install or register other
clients. Keep output private: it includes WHOOP credentials and telemetry
settings. Restrict config file permissions and never commit credentials.
Printed shell commands use POSIX quoting, not PowerShell/cmd.exe syntax.
The binary's absolute path must remain accessible to the client; no Node.js
runtime is needed for the Rust server.

For Perplexity, install its helper, open Plugins > Add custom > Simple, name the
server `whoop`, paste the printed `env ...` command, save, wait for **Running**,
and enable it under **Sources**. Non-macOS/web clients need Perplexity's remote
connector workflow and appropriate account access instead.

For OpenClaw, use a current version with native MCP support, not the separate
mcporter registry. Merge the fragment in Settings > MCP's scoped config editor.
The binary and WHOOP authorization must exist on the **Gateway host**; a path on
your laptop is not sufficient for a remote Gateway. Then run
`openclaw mcp doctor whoop --probe`.

### Claude web/mobile and ChatGPT quick setup

```bash
whoop-mcp setup --client=claude-web
whoop-mcp setup --client=chatgpt
whoop-mcp setup --client=grok
```

These targets print **guidance only**. They do not prompt for secrets, write
configuration, deploy a server, or verify a client connection. Local-only flags
(`--verify`, credential flags, `--config-path`, `--telemetry`) are rejected.
Availability depends on account, plan and workspace administrator policies.

Deploy with `MCP_TRANSPORT=http` behind public HTTPS, keep `MCP_AUTH_TOKEN`
authentication enabled, and securely complete WHOOP authorization on the host.
Use `https://YOUR_HOST/mcp` as the endpoint, replacing the placeholder. Cloud
clients cannot launch the local binary or reach your `localhost`.
For OAuth clients, set `PUBLIC_URL`, `MCP_CONNECTOR_PASSWORD`, and an exact
`ALLOWED_REDIRECT_URIS` allowlist. The existing connector registers **one static
client**, default `whoop-mcp-connector` (`MCP_OAUTH_CLIENT_ID`): enter that ID in
the client where supported. Clients requiring dynamic registration or another
OAuth flow may need server changes outside this setup feature. Do not disable
authentication to bypass incompatibilities.

In ChatGPT, use a custom MCP app/connector with developer mode enabled and
configure compatible OAuth; in Claude, add a custom connector under Settings >
Connectors. Grok instructions apply to the xAI API's remote MCP tool, using its
`authorization` field for `MCP_AUTH_TOKEN`, **not** a WHOOP token or the separate
xAI API key. This is not certification of consumer Grok chat support.

The [shared deployment guide](https://github.com/shashankswe2020-ux/whoop-mcp/blob/main/references/deployment.md)
describes HTTPS, private persistent tokens, proxy and OAuth requirements.
Its Docker/Node commands are for npm; run the Rust binary on your host instead.
Neither implementation is a multi-user WHOOP relay.

Configuration is based on official documentation; tests validate generated
output, not live client accounts:
[Claude](https://support.claude.com/en/articles/11175166-get-started-with-custom-connectors-using-remote-mcp),
[ChatGPT](https://help.openai.com/en/articles/12584461-connecting-custom-mcp-servers-in-chatgpt),
[xAI](https://docs.x.ai/developers/tools/remote-mcp),
[Perplexity](https://www.perplexity.ai/help-center/en/articles/11502712-local-and-remote-mcps-for-perplexity),
[Gemini CLI](https://geminicli.com/docs/tools/mcp-server/),
[Cursor](https://cursor.com/docs/context/mcp),
[VS Code](https://code.visualstudio.com/docs/copilot/customization/mcp-servers),
[OpenClaw](https://docs.openclaw.ai/tools/mcp).

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
