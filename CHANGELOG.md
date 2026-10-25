# Changelog

## Unreleased

- Extend setup with Gemini CLI, Cursor, VS Code, OpenClaw and Perplexity macOS
  configurations, plus the `claude` alias for Claude Desktop.
- Add guidance-only targets for Claude web/mobile, ChatGPT and Grok's xAI API,
  explicitly distinguishing remote hosting/OAuth prerequisites from local setup.
- Preserve existing clients, telemetry defaults, transports, OAuth and token
  storage. Match the npm implementation's targets and platform limitations.

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
