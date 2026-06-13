// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver — single-binary entrypoint.
//!
//! Subcommands wire together the server, terminal cockpit, MCP server, admin
//! tools, and benchmarks. `serve` is live; the others land with their Phase 1
//! features.

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
    Tui,
    /// Run the MCP server for AI agents.
    Mcp,
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
        Command::Tui => println!("quiver tui: not yet implemented"),
        Command::Mcp => println!("quiver mcp: not yet implemented"),
        Command::Admin => println!("quiver admin: not yet implemented"),
        Command::Bench => println!("quiver bench: not yet implemented"),
    }
    Ok(())
}
