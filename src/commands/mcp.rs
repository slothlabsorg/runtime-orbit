//! `runtime-orbit mcp` — start the stdio MCP server (see `crate::mcp`).

use anyhow::Result;

pub async fn run() -> Result<()> {
    crate::mcp::serve().await
}
