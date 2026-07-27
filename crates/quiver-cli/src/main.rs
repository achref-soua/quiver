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
        /// Quiver config file supplying [embedding.*]/[rerank.*] provider tables
        /// for the upsert_text/search_text tools. A missing file is fine — those
        /// tools are then advertised but report no provider configured.
        #[arg(long, env = "QUIVER_CONFIG", default_value = "quiver.toml")]
        config: PathBuf,
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
            let config = quiver_server::Config::load()?;
            quiver_server::init_observability(&config);
            let result = quiver_server::run(config).await;
            // Flush any batched OTLP spans before exiting, even on error.
            quiver_server::shutdown_observability();
            result?;
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
            config,
        } => {
            quiver_mcp::run_with_config(
                &data_dir,
                encryption_key.as_deref(),
                insecure,
                Some(&config),
            )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    // Every flag, default and env binding below is a **public contract**: a user's
    // scripts, systemd units and Dockerfiles are written against them, and 1.0
    // promises they keep working. clap derives them, so a refactor can rename or
    // drop one silently — nothing else in the tree references these strings. These
    // tests are what makes that break loudly.

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // clap's own audit: duplicate flags, conflicting shorts, bad defaults.
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_takes_no_arguments() {
        let cli = Cli::try_parse_from(["quiver", "serve"]).unwrap();
        assert!(matches!(cli.command, Command::Serve));
    }

    #[test]
    fn tui_defaults_to_loopback_and_no_key() {
        let cli = Cli::try_parse_from(["quiver", "tui"]).unwrap();
        let Command::Tui { url, api_key } = cli.command else {
            panic!("expected tui");
        };
        assert_eq!(url, "http://127.0.0.1:6333");
        assert!(api_key.is_none());
    }

    #[test]
    fn tui_accepts_an_explicit_url_and_key() {
        let cli = Cli::try_parse_from(["quiver", "tui", "--url", "http://h:1/", "--api-key", "k"])
            .unwrap();
        let Command::Tui { url, api_key } = cli.command else {
            panic!("expected tui");
        };
        assert_eq!(url, "http://h:1/");
        assert_eq!(api_key.as_deref(), Some("k"));
    }

    #[test]
    fn mcp_defaults_are_secure_by_default() {
        let cli = Cli::try_parse_from(["quiver", "mcp"]).unwrap();
        let Command::Mcp {
            data_dir,
            encryption_key,
            insecure,
            config,
        } = cli.command
        else {
            panic!("expected mcp");
        };
        assert_eq!(data_dir, PathBuf::from("./data"));
        assert_eq!(config, PathBuf::from("quiver.toml"));
        assert!(encryption_key.is_none());
        // The one that matters: encryption-at-rest is never off unless asked for.
        assert!(!insecure, "insecure must default to false");
    }

    #[test]
    fn update_defaults_to_installing_not_just_checking() {
        let cli = Cli::try_parse_from(["quiver", "update"]).unwrap();
        assert!(matches!(cli.command, Command::Update { check: false }));
        let cli = Cli::try_parse_from(["quiver", "update", "--check"]).unwrap();
        assert!(matches!(cli.command, Command::Update { check: true }));
    }

    #[test]
    fn admin_import_parses_the_offline_form() {
        let cli = Cli::try_parse_from([
            "quiver",
            "admin",
            "import",
            "--source",
            "qdrant",
            "--input",
            "export.jsonl",
            "--collection",
            "items",
            "--dim",
            "8",
        ])
        .unwrap();
        let Command::Admin { command } = cli.command else {
            panic!("expected admin");
        };
        let AdminCommand::Import {
            source,
            input,
            collection,
            dim,
            insecure,
            ..
        } = *command;
        assert_eq!(source, "qdrant");
        assert_eq!(input.as_deref(), Some(std::path::Path::new("export.jsonl")));
        assert_eq!(collection, "items");
        assert_eq!(dim, Some(8));
        assert!(
            !insecure,
            "imports are encrypted at rest unless asked otherwise"
        );
    }

    #[test]
    fn admin_import_parses_the_live_connector_form() {
        let cli = Cli::try_parse_from([
            "quiver",
            "admin",
            "import",
            "--source",
            "chroma",
            "--chroma-url",
            "http://127.0.0.1:8000",
            "--collection",
            "docs",
        ])
        .unwrap();
        let Command::Admin { command } = cli.command else {
            panic!("expected admin");
        };
        let AdminCommand::Import {
            source, chroma_url, ..
        } = *command;
        assert_eq!(source, "chroma");
        assert_eq!(chroma_url.as_deref(), Some("http://127.0.0.1:8000"));
    }

    #[test]
    fn an_unknown_subcommand_or_flag_is_rejected() {
        assert!(Cli::try_parse_from(["quiver", "nope"]).is_err());
        assert!(Cli::try_parse_from(["quiver", "tui", "--nope"]).is_err());
        // A subcommand is required — bare `quiver` must not silently do nothing.
        assert!(Cli::try_parse_from(["quiver"]).is_err());
    }

    #[test]
    fn admin_import_requires_a_source_and_collection() {
        assert!(Cli::try_parse_from(["quiver", "admin", "import"]).is_err());
        assert!(
            Cli::try_parse_from(["quiver", "admin", "import", "--source", "qdrant"]).is_err(),
            "a collection name is required"
        );
    }

    #[test]
    fn the_styles_helper_builds() {
        // Exercised only by the derive at startup otherwise.
        let _ = quiver_styles();
    }
}
