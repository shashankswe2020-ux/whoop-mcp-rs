# Changelog

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
