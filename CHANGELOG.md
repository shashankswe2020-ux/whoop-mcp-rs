# Changelog

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
