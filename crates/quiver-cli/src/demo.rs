// SPDX-License-Identifier: AGPL-3.0-only
//! `quiver demo` — zero-config demo mode.
//!
//! One command after install: seeds 1 000 synthetic 128-d vectors, starts
//! the REST server on :7333, and opens the TUI cockpit — no env vars, no
//! config files, no external downloads needed.
//!
//! Override the data directory: `QUIVER_DEMO_DIR=/path/to/dir quiver demo`
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

const DEMO_KEY: &str = "quiver-demo";
const DEMO_DIM: u32 = 128;
const DEMO_POINTS: usize = 1_000;
const DEMO_COLLECTION: &str = "demo";
const SERVER_PORT: u16 = 7333;

// ── colour helpers ────────────────────────────────────────────────────────────

fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err() && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

fn colored(code: &str, text: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn banner(version: &str) {
    println!();
    if use_color() {
        let b = "\x1b[38;2;205;127;50m"; // #CD7F32 bronze — theme CHROME
        let v = "\x1b[38;2;63;182;168m"; // #3FB6A8 verdigris — theme ACCENT (the V arrowhead)
        let r = "\x1b[0m";
        println!("{b}    ██████╗ ██╗   ██╗██╗{r}{v}██╗   ██╗{r}{b}███████╗██████╗ {r}");
        println!("{b}   ██╔═══██╗██║   ██║██║{r}{v}██║   ██║{r}{b}██╔════╝██╔══██╗{r}");
        println!("{b}   ██║   ██║██║   ██║██║{r}{v}╚██╗ ██╔╝{r}{b}█████╗  ██████╔╝{r}");
        println!("{b}   ██║▄▄ ██║██║   ██║██║{r}{v} ╚████╔╝ {r}{b}██╔══╝  ██╔══██╗{r}");
        println!("{b}   ╚██████╔╝╚██████╔╝██║{r}{v}  ╚██╔╝  {r}{b}███████╗██║  ██║{r}");
        println!("{b}    ╚══▀▀═╝  ╚═════╝ ╚═╝{r}{v}   ╚═╝   {r}{b}╚══════╝╚═╝  ╚═╝{r}");
        println!("{v}        demo  ·  v{version}  ·  :{SERVER_PORT}{r}");
        println!();
        println!("\x1b[38;2;90;90;90m  ┌─────────────────────────────────────────────────┐\x1b[0m");
        println!("\x1b[38;2;90;90;90m  │  zero config  ·  press q in the cockpit to quit │\x1b[0m");
        println!("\x1b[38;2;90;90;90m  └─────────────────────────────────────────────────┘\x1b[0m");
    } else {
        println!("  QUIVER v{version}  demo  :{SERVER_PORT}");
        println!("  zero config | press q to quit");
    }
    println!();
}

fn step(icon: &str, msg: &str) {
    print!("  {}  {msg} ", colored("38;2;63;182;168", icon));
    let _ = io::stdout().flush();
}

fn done() {
    println!("{}", colored("38;2;143;179;57", "done"));
}

fn ok(msg: &str) {
    println!("  {}  {msg}", colored("38;2;143;179;57", "✔"));
}

// ── synthetic vectors — no rand dep ──────────────────────────────────────────
// xorshift-based LCG; produces non-trivial values for HNSW to work with.

fn synthetic_vector(point_index: usize, dim: usize) -> Vec<f32> {
    let mut s = point_index
        .wrapping_mul(6364136223846793005_usize)
        .wrapping_add(1442695040888963407_usize);
    (0..dim)
        .map(|d| {
            s = s
                .wrapping_mul(6364136223846793005_usize)
                .wrapping_add(d.wrapping_mul(2891336453_usize) ^ point_index);
            (s as i64 as f64 / i64::MAX as f64) as f32
        })
        .collect()
}

// ── demo data directory ───────────────────────────────────────────────────────
// Override with QUIVER_DEMO_DIR if the default location is not writable.

fn demo_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("QUIVER_DEMO_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("quiver-demo")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
            .join("quiver-demo")
    }
}

// ── seeding ───────────────────────────────────────────────────────────────────

fn open_db(data_dir: &Path) -> Result<quiver_embed::Database> {
    use quiver_embed::Database;

    // First attempt.
    if let Ok(db) = Database::open(data_dir) {
        return Ok(db);
    }

    // On Windows, a stale WAL lock from a previous crash (or a brief antivirus
    // scan hold) can make the first open fail.  Clear the directory and retry
    // once — demo data is synthetic so it is safe to regenerate.
    let _ = std::fs::remove_dir_all(data_dir);
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create demo data dir {}", data_dir.display()))?;

    Database::open(data_dir).with_context(|| {
        format!(
            "cannot open demo database at {}.\n  \
             Set QUIVER_DEMO_DIR=<path> to use a different location, or\n  \
             on Windows add quiver.exe to Windows Security exclusions.",
            data_dir.display()
        )
    })
}

fn seed_demo(data_dir: &Path) -> Result<bool> {
    use quiver_embed::{Descriptor, DistanceMetric, Dtype};

    let mut db = open_db(data_dir)?;

    if db.collection_names().iter().any(|n| n == DEMO_COLLECTION) {
        return Ok(false); // already seeded
    }

    let descriptor = Descriptor::new(DEMO_DIM, Dtype::F32, DistanceMetric::Cosine);
    db.create_collection(DEMO_COLLECTION, descriptor)
        .context("failed to create demo collection")?;

    let vecs: Vec<Vec<f32>> = (0..DEMO_POINTS)
        .map(|i| synthetic_vector(i, DEMO_DIM as usize))
        .collect();
    let payloads: Vec<serde_json::Value> = (0..DEMO_POINTS)
        .map(|i| serde_json::json!({ "index": i, "label": format!("point-{i}") }))
        .collect();
    let ids: Vec<String> = (0..DEMO_POINTS).map(|i| i.to_string()).collect();

    let records: Vec<(&str, &[f32], &serde_json::Value)> = ids
        .iter()
        .zip(vecs.iter())
        .zip(payloads.iter())
        .map(|((id, vec), p)| (id.as_str(), vec.as_slice(), p))
        .collect();

    db.upsert_batch(DEMO_COLLECTION, &records)
        .context("failed to seed demo vectors")?;

    Ok(true)
}

// ── entry point ───────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    banner(version);

    let data_dir = demo_data_dir();
    std::fs::create_dir_all(&data_dir).context("failed to create demo data directory")?;

    step(
        "⟳",
        &format!("Seeding {DEMO_POINTS} vectors into '{DEMO_COLLECTION}'..."),
    );
    let dd = data_dir.clone();
    let seeded = tokio::task::spawn_blocking(move || seed_demo(&dd)).await??;
    done();
    if seeded {
        ok(&format!(
            "{DEMO_POINTS} vectors ready in '{DEMO_COLLECTION}'."
        ));
    } else {
        ok("Existing demo data found — skipping seed.");
    }

    step("⟳", &format!("Starting server on :{SERVER_PORT}..."));
    let dd = data_dir.clone();
    tokio::spawn(async move {
        let config = quiver_server::Config {
            data_dir: dd,
            rest_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SERVER_PORT),
            grpc_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SERVER_PORT + 1),
            api_keys: vec![quiver_server::ApiKey::admin(DEMO_KEY)],
            insecure: true,
            ..Default::default()
        };
        if let Err(e) = quiver_server::run(config).await {
            eprintln!("demo server: {e}");
        }
    });

    thread::sleep(Duration::from_millis(600));
    done();
    ok(&format!("Server ready — http://127.0.0.1:{SERVER_PORT}"));

    println!();
    println!(
        "  {}  {}",
        colored("38;2;90;90;90", "API key"),
        colored("38;2;63;182;168", DEMO_KEY)
    );
    println!(
        "  {}  pip install quiver-client",
        colored("38;2;90;90;90", "Python")
    );
    println!();
    println!(
        "  {}",
        colored("38;2;143;179;57", "Opening cockpit — press q to quit.")
    );
    println!();

    quiver_tui::run(quiver_tui::TuiOptions {
        base_url: format!("http://127.0.0.1:{SERVER_PORT}"),
        api_key: Some(DEMO_KEY.to_string()),
    })
    .await?;

    Ok(())
}
