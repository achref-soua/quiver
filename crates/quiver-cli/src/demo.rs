// SPDX-License-Identifier: AGPL-3.0-only
//! `quiver demo` — zero-config demo mode.
//!
//! One command after install: seeds two collections — `articles` (64 titles with
//! author/topic/year payloads, text-searchable via the built-in `fake` embedder)
//! and `demo` (1 000 synthetic 128-d vectors for the constellation view) — starts
//! the REST server on :7333, and opens the TUI cockpit. No env vars, config
//! files, or external downloads, and the cockpit's every op (browse, constellation,
//! text search) works offline against this data.
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
// A second, text-searchable collection so the cockpit has more than one
// collection to browse and the query runner has rich payloads to inspect. Text
// search is backed by the built-in `fake` embedder (a deterministic content
// hash, wired up in `run()`), so the search op works offline with no model.
const ARTICLES_COLLECTION: &str = "articles";
const ARTICLES_DIM: u32 = 64;
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

// A small, deterministic article corpus: 64 unique titles with author/topic/year
// payloads. Built from word lists so it needs no bundled data file and is stable
// across runs (same ids, same vectors).
fn article_corpus() -> Vec<(String, &'static str, &'static str, u32)> {
    const ADJ: [&str; 8] = [
        "scalable",
        "memory-frugal",
        "secure",
        "fast",
        "elegant",
        "robust",
        "minimal",
        "concurrent",
    ];
    const NOUN: [&str; 8] = [
        "index", "engine", "protocol", "cipher", "cluster", "kernel", "pipeline", "store",
    ];
    const TOPIC: [&str; 8] = [
        "vector databases",
        "rust systems",
        "machine learning",
        "distributed systems",
        "cryptography",
        "search engines",
        "graph algorithms",
        "data structures",
    ];
    const AUTHOR: [&str; 6] = [
        "A. Soua",
        "R. Vega",
        "M. Lin",
        "K. Okafor",
        "S. Petrov",
        "J. Haddad",
    ];
    let mut out = Vec::with_capacity(ADJ.len() * NOUN.len());
    for (a, adj) in ADJ.iter().enumerate() {
        for (n, noun) in NOUN.iter().enumerate() {
            let i = out.len();
            let topic = TOPIC[(a + n) % TOPIC.len()];
            out.push((
                format!("The {adj} {noun} for {topic}"),
                AUTHOR[i % AUTHOR.len()],
                topic,
                2018 + (i % 8) as u32,
            ));
        }
    }
    out
}

// Seed the `articles` collection: embed each title with the same deterministic
// `fake` embedder the server uses for `/query/text` (wired in `run()`), so a
// typed query lands in the same vector space as the stored titles and the
// cockpit's query runner returns real, stable results offline.
fn seed_articles(db: &mut quiver_embed::Database) -> Result<()> {
    use quiver_embed::{Descriptor, DistanceMetric, Dtype};
    use quiver_providers::{EmbeddingProvider, FakeEmbedder};

    if db
        .collection_names()
        .iter()
        .any(|n| n == ARTICLES_COLLECTION)
    {
        return Ok(());
    }
    let descriptor = Descriptor::new(ARTICLES_DIM, Dtype::F32, DistanceMetric::Cosine);
    db.create_collection(ARTICLES_COLLECTION, descriptor)
        .context("failed to create demo articles collection")?;

    let corpus = article_corpus();
    let titles: Vec<String> = corpus.iter().map(|(t, ..)| t.clone()).collect();
    let vectors = FakeEmbedder::new(ARTICLES_DIM as usize)
        .embed(&titles)
        .map_err(|e| anyhow::anyhow!("fake embed failed: {e}"))?;
    let payloads: Vec<serde_json::Value> = corpus
        .iter()
        .map(|(title, author, topic, year)| {
            serde_json::json!({ "title": title, "author": author, "topic": topic, "year": year })
        })
        .collect();
    let ids: Vec<String> = (0..corpus.len()).map(|i| format!("art-{i}")).collect();
    let records: Vec<(&str, &[f32], &serde_json::Value)> = ids
        .iter()
        .zip(vectors.iter())
        .zip(payloads.iter())
        .map(|((id, v), p)| (id.as_str(), v.as_slice(), p))
        .collect();
    db.upsert_batch(ARTICLES_COLLECTION, &records)
        .context("failed to seed demo articles")?;
    Ok(())
}

fn seed_demo(data_dir: &Path) -> Result<bool> {
    use quiver_embed::{Descriptor, DistanceMetric, Dtype};

    let mut db = open_db(data_dir)?;

    if db.collection_names().iter().any(|n| n == DEMO_COLLECTION) {
        return Ok(false); // already seeded
    }

    // The text-searchable collection first, so the browser opens on a collection
    // whose query runner works out of the box.
    seed_articles(&mut db)?;

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
        &format!(
            "Seeding '{ARTICLES_COLLECTION}' (text search) and {DEMO_POINTS} vectors into '{DEMO_COLLECTION}'..."
        ),
    );
    let dd = data_dir.clone();
    let seeded = tokio::task::spawn_blocking(move || seed_demo(&dd)).await??;
    done();
    if seeded {
        ok(&format!(
            "'{ARTICLES_COLLECTION}' ready for text search · {DEMO_POINTS} vectors ready in '{DEMO_COLLECTION}' for the constellation."
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
            // Wire the built-in deterministic `fake` embedder to the articles
            // collection so the cockpit's text search (`POST /query/text`) works
            // with no external model — the same embedder used to seed it.
            embedding: std::collections::HashMap::from([(
                ARTICLES_COLLECTION.to_string(),
                quiver_server::EmbeddingConfig {
                    provider: quiver_server::ProviderKind::Fake,
                    model: String::new(),
                    endpoint: String::new(),
                    dim: ARTICLES_DIM,
                    api_key_env: String::new(),
                },
            )]),
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    // The demo database covers every cockpit op: two collections to browse, a
    // 1 000-point collection for the constellation, and a text-searchable
    // collection whose vectors are embedded with the same `fake` embedder the
    // server uses for `/query/text` — so an exact-title query returns its article.
    #[test]
    fn seed_demo_creates_both_collections_and_articles_are_searchable() {
        use quiver_embed::SearchParams;
        use quiver_providers::{EmbeddingProvider, FakeEmbedder};

        let tmp = tempfile::tempdir().unwrap();
        assert!(seed_demo(tmp.path()).unwrap(), "first seed reports seeded");

        let mut db = quiver_embed::Database::open(tmp.path()).unwrap();
        let names = db.collection_names();
        assert!(
            names.iter().any(|n| n == ARTICLES_COLLECTION),
            "articles collection seeded"
        );
        assert!(
            names.iter().any(|n| n == DEMO_COLLECTION),
            "demo collection seeded"
        );

        let corpus = article_corpus();
        assert_eq!(db.len(ARTICLES_COLLECTION).unwrap(), corpus.len());
        assert_eq!(db.len(DEMO_COLLECTION).unwrap(), DEMO_POINTS);

        // The cockpit's text search embeds the query with this same `fake` embedder,
        // so querying an exact stored title returns that article first.
        let (title0, ..) = corpus[0].clone();
        let qv = FakeEmbedder::new(ARTICLES_DIM as usize)
            .embed(&[title0])
            .unwrap()
            .remove(0);
        let hits = db
            .search(
                ARTICLES_COLLECTION,
                &qv,
                &SearchParams {
                    k: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(hits[0].id, "art-0", "exact-title query returns its article");

        // Re-seeding an existing store is a no-op.
        assert!(!seed_demo(tmp.path()).unwrap(), "second seed is a no-op");
    }
}
