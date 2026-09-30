//! Newline-delimited JSON-RPC over stdin/stdout.

use crate::server::McpServer;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// Serve MCP over arbitrary byte streams until the input ends.
pub async fn serve<R, W>(server: Arc<McpServer>, input: R, output: W)
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let output = Arc::new(Mutex::new(output));
    let mut lines = BufReader::new(input).lines();
    let mut tasks = JoinSet::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let server = server.clone();
        let output = output.clone();
        tasks.spawn(async move {
            if let Some(response) = server.handle(&message).await {
                let mut text = crate::js::stringify(&response);
                text.push('\n');
                let mut out = output.lock().await;
                let _ = out.write_all(text.as_bytes()).await;
                let _ = out.flush().await;
            }
        });
        while tasks.try_join_next().is_some() {}
    }
    while tasks.join_next().await.is_some() {}
}

/// Serve MCP over the process stdin/stdout.
pub async fn serve_stdio(server: Arc<McpServer>) {
    serve(server, tokio::io::stdin(), tokio::io::stdout()).await;
}
