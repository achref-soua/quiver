// SPDX-License-Identifier: AGPL-3.0-only
//! Quiver — single-binary entrypoint.
//!
//! Subcommands wire together the server, terminal cockpit, MCP server, admin
//! tools, and benchmarks. `serve` is live; the others land with their Phase 1
//! features.

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Color, RgbColor, Style, Styles};
use clap::{Parser, Subcommand};

/// Bronze/verdigris clap style — keeps every help screen on-theme.
fn quiver_styles() -> Styles {
    // 24-bit true-color via anstyle RgbColor.
    let bronze = Style::new()
        .fg_color(Some(Color::Rgb(RgbColor(205, 127, 50))))
        .bold();
    let verdigris = Style::new()
        .fg_color(Some(Color::Rgb(RgbColor(63, 182, 168))))
        .bold();
    let ok_green = Style::new().fg_color(Some(Color::Rgb(RgbColor(143, 179, 57))));
    let dark_gray = Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlack)));
    Styles::styled()
        .header(bronze)
        .usage(bronze)
        .literal(verdigris)
        .placeholder(ok_green)
        .error(
            Style::new()
                .fg_color(Some(Color::Rgb(RgbColor(210, 85, 47))))
                .bold(),
        )
        .valid(ok_green)
        .invalid(dark_gray)
}

mod admin;
mod demo;
mod update;

#[derive(Parser)]
#[command(
    name = "quiver",
    version,
    about = "Security-first, memory-frugal vector database",
    styles = quiver_styles(),
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
    /// Administrative commands (imports, collections, keys).
    Admin {
        // Boxed to keep this large, rarely-built subcommand from bloating every
        // `Command` value (clippy::large_enum_variant); the enum is parsed once.
        #[command(subcommand)]
        command: Box<AdminCommand>,
    },
    /// Run benchmarks.
    Bench,
    /// Check for a newer release and optionally install it.
    Update {
        /// Only check whether an update is available; do not download or install.
        #[arg(long)]
        check: bool,
    },
    /// Zero-config demo: seeds 1 000 vectors, starts the server, opens the cockpit.
    Demo,
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Import an export from another vector database into a collection (ADR-0024).
    Import {
        /// Source tool: qdrant, chroma, or pgvector.
        #[arg(long)]
        source: String,
        /// Export file (offline import): JSON Lines for qdrant/pgvector; a single
        /// `collection.get(...)` JSON object for chroma.
        #[arg(long)]
        input: Option<PathBuf>,
        /// Live import: base URL of a running Qdrant (e.g. http://localhost:6333)
        /// to pull the same-named collection directly, instead of `--input`.
        #[arg(long)]
        qdrant_url: Option<String>,
        /// Live import: base URL of a running Chroma (e.g. http://localhost:8000)
        /// to pull the same-named collection over its v2 API.
        #[arg(long)]
        chroma_url: Option<String>,
        /// Chroma tenant for `--chroma-url` (default: default_tenant).
        #[arg(long)]
        chroma_tenant: Option<String>,
        /// Chroma database for `--chroma-url` (default: default_database).
        #[arg(long)]
        chroma_database: Option<String>,
        /// Live import: Postgres URL (postgresql://user:pass@host/db) to pull
        /// pgvector rows directly, instead of `--input`.
        #[arg(long)]
        postgres_url: Option<String>,
        /// Source table for `--postgres-url` (defaults to `--collection`).
        #[arg(long)]
        table: Option<String>,
        /// API key for a live import: Qdrant `api-key` or Chroma `x-chroma-token`.
        #[arg(long, env = "QDRANT_API_KEY")]
        api_key: Option<String>,
        /// Target collection name (created if absent, appended to otherwise).
        #[arg(long)]
        collection: String,
        /// Data directory for the embedded database.
        #[arg(long, env = "QUIVER_DATA_DIR", default_value = "./data")]
        data_dir: PathBuf,
        /// Distance metric for a newly created collection (l2, cosine, or dot).
        #[arg(long, default_value = "cosine")]
        metric: String,
        /// Vector dimensionality (inferred from the export when omitted).
        #[arg(long)]
        dim: Option<usize>,
        /// Filterable payload field as `path:type` (type = keyword|numeric); repeatable.
        #[arg(long = "filterable", value_name = "PATH:TYPE")]
        filterable: Vec<String>,
        /// Id column name (pgvector; defaults to `id`).
        #[arg(long)]
        id_field: Option<String>,
        /// Vector column name (defaults: qdrant `vector`, pgvector `embedding`).
        #[arg(long)]
        vector_field: Option<String>,
        /// Named vector to import (for qdrant collections with named vectors).
        #[arg(long)]
        vector_name: Option<String>,
        /// 64-hex-character master key for encryption-at-rest (or QUIVER_ENCRYPTION_KEY).
        #[arg(long, env = "QUIVER_ENCRYPTION_KEY")]
        encryption_key: Option<String>,
        /// Import into an unencrypted database (development only).
        #[arg(long, env = "QUIVER_INSECURE", default_value_t = false)]
        insecure: bool,
    },
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
        Command::Admin { command } => match *command {
            AdminCommand::Import {
                source,
                input,
                qdrant_url,
                chroma_url,
                chroma_tenant,
                chroma_database,
                postgres_url,
                table,
                api_key,
                collection,
                data_dir,
                metric,
                dim,
                filterable,
                id_field,
                vector_field,
                vector_name,
                encryption_key,
                insecure,
            } => {
                let args = admin::ImportArgs {
                    source: source.parse().map_err(|e: String| anyhow::anyhow!(e))?,
                    input,
                    qdrant_url,
                    chroma_url,
                    chroma_tenant,
                    chroma_database,
                    postgres_url,
                    table,
                    api_key,
                    collection: collection.clone(),
                    data_dir,
                    metric: admin::parse_metric(&metric)?,
                    dim,
                    filterable: admin::parse_filterable(&filterable)?,
                    id_field,
                    vector_field,
                    vector_name,
                    encryption_key,
                    insecure,
                };
                let n = admin::import(args)?;
                println!("imported {n} points into collection '{collection}'");
            }
        },
        Command::Bench => println!("quiver bench: not yet implemented"),
        Command::Update { check } => update::run(check).await?,
        Command::Demo => demo::run().await?,
    }
    Ok(())
}
