// Sends an identical HTTP request sequence to a running server and prints
// normalized responses as JSON, for diffing the TypeScript and Rust servers.
//
// Usage: node scripts/http-transcript.mjs http://127.0.0.1:PORT > out.json
// Server env: MCP_TRANSPORT=http MCP_AUTH_TOKEN=test-token-0123456789 MCP_TRUST_PROXY=1
//   MCP_ALLOWED_ORIGINS=https://app.example MCP_CONNECTOR_PASSWORD='correct horse battery'
//   PUBLIC_URL=https://mcp.example.com ALLOWED_REDIRECT_URIS=https://claude.ai/api/mcp/auth_callback

import { createHash } from "node:crypto";

const base = process.argv[2];
const TOKEN = "test-token-0123456789";
const REDIRECT = "https://claude.ai/api/mcp/auth_callback";
const PASSWORD = "correct horse battery";
const KEEP_HEADERS = [
  "content-type",
  "cache-control",
  "allow",
  "access-control-allow-origin",
  "access-control-allow-methods",
  "access-control-allow-headers",
  "access-control-expose-headers",
  "retry-after",
  "ratelimit-policy",
  "ratelimit-limit",
  "ratelimit-remaining",
  "x-frame-options",
  "content-security-policy",
  "referrer-policy",
  "x-content-type-options",
  "x-accel-buffering",
];

let ipCounter = 0;
const freshIp = () => `198.51.100.${++ipCounter}`;
const mask = (text) =>
  text
    .replace(/code=[A-Za-z0-9_-]{20,}/g, "code=<code>")
    .replace(/"(access_token|refresh_token)":"[^"]+"/g, '"$1":"<jwt>"')
    .replace(/"uptime":\d+/g, '"uptime":<n>');

async function send(label, path, init = {}) {
  const response = await fetch(`${base}${path}`, { redirect: "manual", ...init });
  const headers = {};
  for (const name of KEEP_HEADERS) {
    const value = response.headers.get(name);
    if (value !== null) headers[name] = value;
  }
  if (response.headers.get("location")) headers.location = mask(response.headers.get("location"));
  if (response.headers.get("mcp-session-id")) headers["mcp-session-id"] = "<session>";
  const body = mask(await response.text());
  return { label, status: response.status, headers, body, raw: response };
}

const out = [];
const record = async (...args) => {
  const result = await send(...args);
  out.push({ label: result.label, status: result.status, headers: result.headers, body: result.body });
  return result;
};
const mcpHeaders = (extra = {}) => ({
  Authorization: `Bearer ${TOKEN}`,
  Accept: "application/json, text/event-stream",
  "Content-Type": "application/json",
  ...extra,
});
const rpc = (method, id, params) => JSON.stringify({ jsonrpc: "2.0", ...(id === undefined ? {} : { id }), method, ...(params ? { params } : {}) });
const form = (pairs) => new URLSearchParams(pairs).toString();
const formHeaders = () => ({ "Content-Type": "application/x-www-form-urlencoded", "X-Forwarded-For": freshIp() });

await record("health public", "/health");
await record("health authed", "/health", { headers: { Authorization: `Bearer ${TOKEN}` } });
await record("health lowercase bearer", "/health", { headers: { Authorization: `bearer ${TOKEN}` } });
await record("unknown route", "/nope");
await record("preflight allowed", "/mcp", { method: "OPTIONS", headers: { Origin: "https://app.example" } });
await record("preflight denied", "/mcp", { method: "OPTIONS", headers: { Origin: "https://evil.example" } });
await record("cors on health", "/health", { headers: { Origin: "https://app.example" } });
await record("mcp no auth", "/mcp", { method: "POST" });
await record("mcp wrong auth", "/mcp", { method: "POST", headers: { Authorization: "Bearer nope" } });
await record("mcp invalid json", "/mcp", { method: "POST", headers: mcpHeaders(), body: "{nope" });
await record("mcp not acceptable", "/mcp", { method: "POST", headers: mcpHeaders({ Accept: "application/json" }), body: rpc("ping", 1) });
await record("mcp wrong content type", "/mcp", { method: "POST", headers: mcpHeaders({ "Content-Type": "text/plain" }), body: rpc("ping", 1) });
await record("mcp invalid rpc", "/mcp", { method: "POST", headers: mcpHeaders(), body: JSON.stringify({ hello: 1 }) });
await record("mcp before init", "/mcp", { method: "POST", headers: mcpHeaders(), body: rpc("ping", 1) });
await record("mcp get before init", "/mcp", { headers: { Authorization: `Bearer ${TOKEN}`, Accept: "text/event-stream" } });
await record("mcp get not acceptable", "/mcp", { headers: { Authorization: `Bearer ${TOKEN}` } });
await record("mcp put", "/mcp", { method: "PUT", headers: { Authorization: `Bearer ${TOKEN}` } });
await record("mcp double init", "/mcp", {
  method: "POST",
  headers: mcpHeaders(),
  body: JSON.stringify([JSON.parse(rpc("initialize", 1, { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "t", version: "1" } })), JSON.parse(rpc("ping", 2))]),
});
const init = await record("mcp initialize", "/mcp", {
  method: "POST",
  headers: mcpHeaders(),
  body: rpc("initialize", 1, { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "t", version: "1" } }),
});
const session = init.raw.headers.get("mcp-session-id");
out[out.length - 1].body = out[out.length - 1].body.replace(/"version":"[^"]+"/, '"version":"<v>"');
await record("mcp notification", "/mcp", { method: "POST", headers: mcpHeaders({ "mcp-session-id": session }), body: rpc("notifications/initialized") });
await record("mcp ping", "/mcp", { method: "POST", headers: mcpHeaders({ "mcp-session-id": session }), body: rpc("ping", 2) });
await record("mcp missing session", "/mcp", { method: "POST", headers: mcpHeaders(), body: rpc("ping", 3) });
await record("mcp unknown session", "/mcp", { method: "POST", headers: mcpHeaders({ "mcp-session-id": "bogus" }), body: rpc("ping", 3) });
await record("mcp bad protocol version", "/mcp", {
  method: "POST",
  headers: mcpHeaders({ "mcp-session-id": session, "mcp-protocol-version": "1999-01-01" }),
  body: rpc("ping", 4),
});
await record("mcp reinitialize", "/mcp", {
  method: "POST",
  headers: mcpHeaders({ "mcp-session-id": session }),
  body: rpc("initialize", 5, { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "t", version: "1" } }),
});
await record("mcp unknown tool", "/mcp", {
  method: "POST",
  headers: mcpHeaders({ "mcp-session-id": session }),
  body: rpc("tools/call", 6, { name: "nope", arguments: {} }),
});
await record("mcp delete", "/mcp", { method: "DELETE", headers: { Authorization: `Bearer ${TOKEN}`, "mcp-session-id": session } });
await record("mcp after delete", "/mcp", { method: "POST", headers: mcpHeaders({ "mcp-session-id": session }), body: rpc("ping", 7) });

await record("as metadata", "/.well-known/oauth-authorization-server");
await record("as metadata post", "/.well-known/oauth-authorization-server", { method: "POST" });
await record("protected resource", "/.well-known/oauth-protected-resource");
await record("unknown well-known", "/.well-known/nope");
await record("register", "/register", { method: "POST" });

const verifier = "a-very-long-code-verifier-string-for-pkce-0123456789";
const challenge = createHash("sha256").update(verifier).digest("base64url");
const authorizeParams = {
  client_id: "whoop-mcp-connector",
  redirect_uri: REDIRECT,
  response_type: "code",
  code_challenge: challenge,
  code_challenge_method: "S256",
  scope: "mcp",
  state: 'st"<x>',
};
await record("authorize page", `/authorize?${form(authorizeParams)}`, { headers: { "X-Forwarded-For": freshIp() } });
await record("authorize page repeated param", `/authorize?state=a&state=b&client_id=c`, { headers: { "X-Forwarded-For": freshIp() } });
await record("authorize put", "/authorize", { method: "PUT", headers: { "X-Forwarded-For": freshIp() } });
await record("authorize wrong password", "/authorize", { method: "POST", headers: formHeaders(), body: form({ ...authorizeParams, connector_password: "wrong" }) });
await record("authorize missing client", "/authorize", { method: "POST", headers: formHeaders(), body: form({ connector_password: PASSWORD }) });
await record("authorize bad client", "/authorize", { method: "POST", headers: formHeaders(), body: form({ client_id: "evil", connector_password: PASSWORD }) });
await record("authorize bad redirect", "/authorize", { method: "POST", headers: formHeaders(), body: form({ ...authorizeParams, redirect_uri: "https://evil.example/cb", connector_password: PASSWORD }) });
await record("authorize invalid redirect url", "/authorize", { method: "POST", headers: formHeaders(), body: form({ ...authorizeParams, redirect_uri: "not a url", connector_password: PASSWORD }) });
await record("authorize bad response type", "/authorize", { method: "POST", headers: formHeaders(), body: form({ ...authorizeParams, response_type: "token", connector_password: PASSWORD }) });
await record("authorize missing challenge", "/authorize", { method: "POST", headers: formHeaders(), body: form({ client_id: "whoop-mcp-connector", response_type: "code", code_challenge_method: "S256", state: "s", connector_password: PASSWORD }) });
await record("authorize empty challenge", "/authorize", { method: "POST", headers: formHeaders(), body: form({ ...authorizeParams, code_challenge: "", connector_password: PASSWORD }) });
const approved = await record("authorize approve", "/authorize", { method: "POST", headers: formHeaders(), body: form({ ...authorizeParams, connector_password: PASSWORD }) });
const code = new URL(approved.raw.headers.get("location")).searchParams.get("code");

const limiterIp = freshIp();
for (let i = 1; i <= 4; i += 1) {
  await record(`authorize limiter ${i}`, "/authorize", { headers: { "X-Forwarded-For": limiterIp } });
}

const token = (label, pairs, headers = formHeaders()) => record(label, "/token", { method: "POST", headers, body: form(pairs) });
await record("token get", "/token", { headers: { "X-Forwarded-For": freshIp() } });
await record("token no body", "/token", { method: "POST", headers: { "X-Forwarded-For": freshIp() } });
await token("token missing client", { grant_type: "authorization_code" });
await token("token bad client", { client_id: "evil", grant_type: "authorization_code" });
await token("token missing grant", { client_id: "whoop-mcp-connector" });
await token("token unsupported grant", { client_id: "whoop-mcp-connector", grant_type: "password" });
await token("token missing verifier", { client_id: "whoop-mcp-connector", grant_type: "authorization_code", code });
await token("token bogus code", { client_id: "whoop-mcp-connector", grant_type: "authorization_code", code: "bogus", code_verifier: verifier });
await token("token wrong verifier", { client_id: "whoop-mcp-connector", grant_type: "authorization_code", code, code_verifier: "wrong" });
await token("token wrong redirect", { client_id: "whoop-mcp-connector", grant_type: "authorization_code", code, code_verifier: verifier, redirect_uri: "https://other.example/cb" });
await token("token replay", { client_id: "whoop-mcp-connector", grant_type: "authorization_code", code, code_verifier: verifier });
await token("token bad refresh", { client_id: "whoop-mcp-connector", grant_type: "refresh_token", refresh_token: "a.b.c" });
await token("token bad resource", { client_id: "whoop-mcp-connector", grant_type: "refresh_token", refresh_token: "x", resource: "not a url" });

process.stdout.write(`${JSON.stringify(out, null, 2)}\n`);
