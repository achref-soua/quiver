// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver — single-binary entrypoint.
//!
//! Subcommands wire together the server, terminal cockpit, MCP server, admin
//! tools, and benchmarks. `serve` is live; the others land with their Phase 1
//! features.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "quiver",
    version,
    about = "Security-first, memory-frugal vector database"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (gRPC + REST).
    Serve,
    /// Launch the terminal cockpit.
    Tui {
        /// REST base URL of the server to inspect.
        #[arg(long, env = "QUIVER_TUI_URL", default_value = "http://127.0.0.1:6333")]
        url: String,
        /// API key presented as a bearer token, if the server requires one.
        #[arg(long, env = "QUIVER_API_KEY")]
        api_key: Option<String>,
    },
    /// Run the MCP server for AI agents (JSON-RPC over stdio).
    Mcp {
        /// Data directory for the embedded database.
        #[arg(long, env = "QUIVER_DATA_DIR", default_value = "./data")]
        data_dir: PathBuf,
        /// 64-hex-character key for encryption-at-rest (or set QUIVER_ENCRYPTION_KEY).
        #[arg(long, env = "QUIVER_ENCRYPTION_KEY")]
        encryption_key: Option<String>,
        /// Run without encryption-at-rest (development only).
        #[arg(long, env = "QUIVER_INSECURE", default_value_t = false)]
        insecure: bool,
    },
    /// Administrative commands (collections, keys).
    Admin,
    /// Run benchmarks.
    Bench,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            quiver_server::init_tracing();
            let config = quiver_server::Config::load()?;
            quiver_server::run(config).await?;
        }
        Command::Tui { url, api_key } => {
            quiver_tui::run(quiver_tui::TuiOptions {
                base_url: url,
                api_key,
            })
            .await?;
        }
        Command::Mcp {
            data_dir,
            encryption_key,
            insecure,
        } => {
            quiver_mcp::run(&data_dir, encryption_key.as_deref(), insecure)?;
        }
        Command::Admin => println!("quiver admin: not yet implemented"),
        Command::Bench => println!("quiver bench: not yet implemented"),
    }
    Ok(())
}
