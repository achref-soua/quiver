// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver — single-binary entrypoint.
//!
//! Subcommands wire together the server, terminal cockpit, MCP server, admin
//! tools, and benchmarks. Status: scaffolding — subcommands are stubs until the
//! corresponding Phase 1 features land.

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

fn main() {
    let cli = Cli::parse();
    let subcommand = match cli.command {
        Command::Serve => "serve",
        Command::Tui => "tui",
        Command::Mcp => "mcp",
        Command::Admin => "admin",
        Command::Bench => "bench",
    };
    println!("quiver {subcommand}: not yet implemented");
}
