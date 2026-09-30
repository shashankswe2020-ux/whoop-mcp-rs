//! Read-only WHOOP MCP server.
//!
//! A Rust implementation of the [`whoop-ai-mcp`](https://www.npmjs.com/package/whoop-ai-mcp)
//! Model Context Protocol server. It exposes WHOOP recovery, sleep, strain,
//! workout, and personal analytics data to AI assistants over stdio or
//! Streamable HTTP, with identical tools, schemas, resources, and prompts.
//!
//! Most users run the `whoop-mcp` binary; the library is exposed for embedding
//! and testing (for example [`server::McpServer`] with a custom [`api::WhoopApi`]).

pub mod api;
pub mod app;
pub mod auth;
pub mod cache;
pub mod catalog;
pub mod cli;
pub mod crypto;
pub mod http_util;
pub mod js;
pub mod logging;
pub mod net;
pub mod prompts;
pub mod resources;
pub mod schema;
pub mod server;
pub mod telemetry;
pub mod tools;
pub mod transport;
