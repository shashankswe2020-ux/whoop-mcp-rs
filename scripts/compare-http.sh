#!/usr/bin/env bash
# Runs scripts/http-transcript.mjs against the TypeScript and Rust servers and diffs the results.
# Usage: scripts/compare-http.sh <path-to-whoop-mcp-checkout>
set -euo pipefail
ts_repo="$(cd "${1:-../whoop-mcp}" && pwd)"
here="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'kill ${ts_pid:-} ${rs_pid:-} 2>/dev/null || true; rm -rf "$work"' EXIT
cargo build --quiet --manifest-path "$here/Cargo.toml"

export HOME="$work/home" MCP_TRANSPORT=http MCP_HOST=127.0.0.1 MCP_AUTH_TOKEN=test-token-0123456789 MCP_TRUST_PROXY=1 \
  MCP_ALLOWED_ORIGINS=https://app.example MCP_CONNECTOR_PASSWORD='correct horse battery' \
  PUBLIC_URL=https://mcp.example.com ALLOWED_REDIRECT_URIS=https://claude.ai/api/mcp/auth_callback \
  WHOOP_CLIENT_ID=fake WHOOP_CLIENT_SECRET=fake WHOOP_MCP_TELEMETRY=0
mkdir -p "$HOME"

MCP_PORT=38731 node "$ts_repo/dist/index.js" 2>"$work/ts.log" & ts_pid=$!
MCP_PORT=38732 "$here/target/debug/whoop-mcp" 2>"$work/rs.log" & rs_pid=$!
sleep 2

node "$here/scripts/http-transcript.mjs" http://127.0.0.1:38731 > "$work/ts.json"
node "$here/scripts/http-transcript.mjs" http://127.0.0.1:38732 > "$work/rs.json"
if diff -u "$work/ts.json" "$work/rs.json"; then
  echo "HTTP transcripts identical ($(grep -c '"label"' "$work/ts.json") requests)"
fi
