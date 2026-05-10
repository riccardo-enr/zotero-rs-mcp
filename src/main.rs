/* zotero-mcp -- Model Context Protocol server exposing the Zotero local
connector API. Reuses the ZoteroClient + types + merge logic from the
sibling zotero-cli crate. Speaks MCP over stdio so it can be wired into
any MCP-aware client (Claude Code, etc.) via a stdio command entry. */

mod client;
mod config;
mod merge;
mod resources;
mod tools;
mod types;

use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    /* Logs go to stderr so they don't corrupt the JSON-RPC stream on stdout */
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("starting zotero-mcp");

    let server = tools::ZoteroServer::new()?;
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(?e, "serve error"))?;
    service.waiting().await?;
    Ok(())
}
