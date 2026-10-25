# Changelog

## Unreleased

## 0.2.0 - 2026-10-25

- Extend setup with Gemini CLI, Cursor, VS Code, OpenClaw and Perplexity macOS
  configurations, plus the `claude` alias for Claude Desktop.
- Add guidance-only targets for Claude web/mobile, ChatGPT and Grok's xAI API,
  explicitly distinguishing remote hosting/OAuth prerequisites from local setup.
- Preserve existing clients, telemetry defaults, transports, OAuth and token
  storage. Match the npm implementation's targets and platform limitations.
- Update Rustls, Tokio and certificate dependencies to address the advisories
  reported by the release dependency audit.

## 0.1.1 - 2026-10-09

- Successful setup now ends with an optional Buy Me a Coffee sponsorship link
  for fresh installs, CLI client registration, and existing Claude Desktop
  configurations.
- Added regression coverage that keeps the sponsorship URL as the final setup
  output line.
- Rustfmt, strict Clippy, all 86 tests, and cross-platform GitHub CI pass.

## 0.1.0

Initial Rust release, at feature parity with `whoop-ai-mcp` 0.8.4.

- 16 read-only tools, 4 resources, and 5 prompts with the same schemas and results
  as the TypeScript server (verified by differential tests).
- stdio and Streamable HTTP transports, with bearer auth, CORS, rate and
  connection limits, and the optional OAuth 2.1 connector for claude.ai.
- WHOOP OAuth with PKCE, shared token storage, cross-process flow locking, and
  automatic token refresh.
- `setup`, `doctor`, and `telemetry status` subcommands; opt-in telemetry.
- Standard and aggregate privacy modes.
