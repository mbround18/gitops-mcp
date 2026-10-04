//! stdio entry point for the gitops MCP server.

mod server;

use anyhow::Result;
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

/// Git signing governance over the Model Context Protocol.
///
/// Speaks MCP on stdin/stdout, so it is normally launched by an MCP client
/// rather than run by hand. Register it with:
/// `claude mcp add --scope user gitops gitops-mcp`
#[derive(Debug, Parser)]
#[command(name = "gitops-mcp", version)]
struct Cli {
    /// Log filter for diagnostics on stderr, in `tracing` syntax (e.g. `debug`).
    #[arg(long, env = "GITOPS_MCP_LOG", default_value = "info")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // stdout carries the protocol; diagnostics go to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting gitops-mcp");
    let service = server::GitOpsServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
