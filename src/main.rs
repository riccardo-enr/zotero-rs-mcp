/* zotero-mcp -- Model Context Protocol server exposing the Zotero local
connector API. Reuses the ZoteroClient + types + merge logic from the
sibling zotero-cli crate.

Default transport is stdio so the binary drops into any MCP-aware client
(Claude Code, etc.) as a stdio command entry. Pass `--http <addr>` (e.g.
`--http 127.0.0.1:8765`) to expose the same MCP surface over rmcp's
streamable-HTTP transport at `/mcp`, which lets the server run as a
daemon and be consumed remotely. */

mod client;
mod config;
mod merge;
mod prompts;
mod resources;
mod tools;
mod types;

use anyhow::{Context, Result};
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

    let args = parse_args(std::env::args().skip(1))?;

    match args.transport {
        Transport::Stdio => run_stdio().await,
        Transport::Http { addr } => run_http(&addr).await,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Transport {
    Stdio,
    Http { addr: String },
}

#[derive(Debug)]
struct CliArgs {
    transport: Transport,
}

/* Parse `--http <addr>` (also accepts `--http=<addr>`). Anything else is
treated as an error so typos don't silently fall through to stdio mode. */
fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<CliArgs> {
    let mut iter = args.into_iter();
    let mut transport = Transport::Stdio;
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--http" => {
                let addr = iter
                    .next()
                    .context("--http requires an address argument, e.g. 127.0.0.1:8765")?;
                transport = Transport::Http { addr };
            }
            s if s.starts_with("--http=") => {
                let addr = s.trim_start_matches("--http=").to_string();
                if addr.is_empty() {
                    anyhow::bail!(
                        "--http= requires a non-empty address, e.g. --http=127.0.0.1:8765"
                    );
                }
                transport = Transport::Http { addr };
            }
            "--help" | "-h" => {
                eprintln!("zotero-mcp [--http <addr>]");
                eprintln!();
                eprintln!("  --http <addr>   Serve MCP over streamable-HTTP at /mcp on <addr>.");
                eprintln!("                  Default is stdio.");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(CliArgs { transport })
}

async fn run_stdio() -> Result<()> {
    tracing::info!("starting zotero-mcp (stdio)");
    let server = tools::ZoteroServer::new()?;
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(?e, "serve error"))?;
    service.waiting().await?;
    Ok(())
}

async fn run_http(addr: &str) -> Result<()> {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio_util::sync::CancellationToken;

    /* Build the MCP service factory once and let StreamableHttpService
    instantiate a fresh ZoteroServer per session. ZoteroServer::new() reads
    config + the storage root, both cheap. */
    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default().with_cancellation_token(ct.clone());
    let service = StreamableHttpService::new(
        || tools::ZoteroServer::new().map_err(std::io::Error::other),
        std::sync::Arc::new(LocalSessionManager::default()),
        config,
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let bound = listener.local_addr().context("local_addr")?;
    tracing::info!("starting zotero-mcp (http) on http://{bound}/mcp");

    /* Cancel pending sessions on Ctrl-C so the process exits cleanly. */
    let shutdown_ct = ct.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            tracing::info!("ctrl-c received, shutting down http transport");
            shutdown_ct.cancel();
        }
    });

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { ct.cancelled_owned().await })
        .await
        .context("axum::serve")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<CliArgs> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_defaults_to_stdio() {
        let a = cli(&[]).unwrap();
        assert_eq!(a.transport, Transport::Stdio);
    }

    #[test]
    fn http_with_separate_arg() {
        let a = cli(&["--http", "127.0.0.1:8765"]).unwrap();
        assert_eq!(
            a.transport,
            Transport::Http {
                addr: "127.0.0.1:8765".into()
            }
        );
    }

    #[test]
    fn http_with_equals_form() {
        let a = cli(&["--http=0.0.0.0:9000"]).unwrap();
        assert_eq!(
            a.transport,
            Transport::Http {
                addr: "0.0.0.0:9000".into()
            }
        );
    }

    #[test]
    fn http_without_addr_errors() {
        let err = cli(&["--http"]).unwrap_err();
        assert!(err.to_string().contains("--http requires an address"));
    }

    #[test]
    fn http_equals_empty_errors() {
        let err = cli(&["--http="]).unwrap_err();
        assert!(err.to_string().contains("non-empty address"));
    }

    #[test]
    fn unknown_flag_errors() {
        let err = cli(&["--bogus"]).unwrap_err();
        assert!(err.to_string().contains("unknown argument"));
    }
}
