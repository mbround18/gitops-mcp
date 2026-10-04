//! stdio entry point for the gitops MCP server.

mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // stdout carries the protocol; diagnostics go to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("GITOPS_MCP_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting gitops-mcp");
    let service = server::GitOpsServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
