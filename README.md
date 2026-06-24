<div align="center">

<img src="docs/assets/cockpit/logo.png" alt="QUIVER — the V is a 3-D arrowhead" width="460">

# Quiver

**The security-first vector database.** Client-side-encryptable, memory-frugal approximate-nearest-neighbour search that runs on a laptop — with a retro terminal cockpit.

[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-stable-orange)](./rust-toolchain.toml)
[![CI](https://img.shields.io/badge/CI-manual%20dispatch-informational)](.github/workflows)
[![release](https://img.shields.io/badge/release-v0.22.0-FFB000)](https://github.com/achref-soua/quiver/releases)
[![status](https://img.shields.io/badge/status-v0.22.0%20·%20phase%205-FFB000)](./docs/roadmap.md)
[![stars](https://img.shields.io/badge/Star_on-GitHub-FFB000?logo=github)](https://github.com/achref-soua/quiver/stargazers)

</div>

> **Status: `v0.22.0` released.** `v0.22.0` — *Complete Quiver*: the **off-lock index rebuild** ([ADR-0062](./docs/adr/0062-rebuild-off-the-exclusive-lock.md)) — a deferred index rebuild no longer stalls concurrent readers; the server serves the **prior snapshot** while the new index builds with **no lock held**, then swaps it in under a brief write lock, with a per-collection write-generation guard so no write is lost (a measured ~77 s reader stall at 100k vectors collapses to the sub-millisecond tail; no `unsafe`, no `loom`); the **interactive TUI cockpit** ([ADR-0060](./docs/adr/0060-interactive-tui-cockpit.md)) — a `/` query runner with a payload inspector and recent-searches, a `?` keybinding overlay, and a `Ctrl-t` Bronze↔Slate theme toggle; an opt-in **OpenTelemetry traces exporter** ([ADR-0059](./docs/adr/0059-otlp-traces-exporter.md)) behind the `otlp` build feature and `QUIVER_OTLP_ENDPOINT` (off by default, no new deps in a normal build); **MCP text tools** `upsert_text` / `search_text` ([ADR-0058](./docs/adr/0058-mcp-text-tools-and-provider-crate.md)) via a shared `quiver-providers` crate, bringing the agent surface to provider parity with REST, gRPC, and the SDKs; the **`quiverdb-*` publish namespace** ([ADR-0056](./docs/adr/0056-packaging-and-distribution.md)) (each crate is `quiverdb-<name>` while its library/extern name and the `quiver` binary are unchanged); and **four new benchmark dimensions** ([ADR-0061](./docs/adr/0061-benchmark-dimensions-v0.22.0.md)) on SIFT1M — recall@{1,10,100}, a saturated-concurrency sweep (1-thread vs 8-thread QPS, up to **1.76× at ef=256**), a quantization tradeoff (the recall@100 tail collapse of disk-Vamana + PQ), and a filtered-selectivity sweep (the planner's recall valley) — every figure traced to a committed CSV in [`comparison-v0.22.0`](./docs/benchmarks/results/comparison-v0.22.0/comparison-v0.22.0.md), with the expanded **"Quiver, Explained"** field guide (committed figures under `docs/assets/explained-figures/`). `v0.21.0` — *Close the Gaps*: **concurrent reads** ([ADR-0057](./docs/adr/0057-concurrent-reads-rwlock.md)) — the server now serves searches behind a reader–writer lock instead of a single mutex, so reads run in parallel; the engine gains `&self` `search_snapshot` reads and `ensure_indexed`, the single-writer model and crash gate unchanged, with the fully lock-free arc-swap snapshot path staged as the successor; a **packaging & distribution pipeline** ([ADR-0056](./docs/adr/0056-packaging-and-distribution.md)) — a `CHANGELOG.md`, crate publish metadata, secret-gated crates.io / PyPI / npm publish jobs, and a **Helm chart + Kubernetes manifests** under `infra/` for one-command self-hosting; and **SDK parity** — the TypeScript client reaches full method parity with the Python async client (`upsertIter` over a sync *or* async iterable, an async-generator `scroll`, `deleteByFilter`), and the Go client gains the same bulk/maintenance helpers (`UpsertBatch` / `Scroll` / `DeleteByFilter`, all context-aware). `v0.20.1` — a documentation & evidence patch (no engine change): the **SIFT1M + GIST1M multi-DB benchmark re-run on the v0.20.0 engine** ([`comparison-v0.20.0`](./docs/benchmarks/results/comparison-v0.20.0/comparison-v0.20.0.md), [ADR-0055](./docs/adr/0055-benchmark-v0.20.0-bulk-build.md)) — Quiver is **second only to FAISS** on SIFT1M (1222 QPS / p95 1.0 ms at recall ≥ 0.95) and **matches FAISS on GIST recall** (0.923 vs 0.919), with the build column now measured through the bulk-ingest path as honest time-until-queryable — folded into the README, methodology, and the "Quiver, Explained" field guide, plus PDF layout polish. `v0.20.0` — *RAG batteries + operability*: **server-side embedding & reranking hooks** ([ADR-0047](./docs/adr/0047-server-side-embedding-and-rerank-hooks.md)) — opt-in, provider-agnostic (OpenAI/Cohere/Ollama/HTTP), per collection, with `upsert_text` / `search_text` on REST, gRPC, and the SDKs; **BM25 full-text** over the sparse path ([ADR-0046](./docs/adr/0046-bm25-full-text.md)) with a dependency-free tokenizer, a **Snowball stemmer** ([ADR-0048](./docs/adr/0048-snowball-stemmer.md)), and `query_text` on every surface; **online snapshot & restore** ([ADR-0050](./docs/adr/0050-snapshot-and-restore.md)) — a consistent whole-database backup over the engine, REST (`POST /v1/snapshot`), the MCP server, and every SDK; a **client-streaming gRPC `UpsertStream`** (the [ADR-0045](./docs/adr/0045-hybrid-everywhere-and-fast-ingest.md) fast-ingest fast-follow); **opt-in per-key rate limiting** ([ADR-0049](./docs/adr/0049-per-key-rate-limiting.md)) — a token bucket with `RateLimit-*` headers and 429 across REST and gRPC; **real Prometheus `/metrics` + request tracing + a Grafana dashboard** ([ADR-0054](./docs/adr/0054-prometheus-metrics-and-tracing.md)); a **standard-library Go SDK**; richer MCP management tools (`database_stats`, `delete_collection`, `snapshot`); and **design ADRs for the big bets** — distributed/sharded mode ([ADR-0051](./docs/adr/0051-distributed-sharded-mode.md)), GPU acceleration ([ADR-0052](./docs/adr/0052-gpu-acceleration.md)), and lock-free MVCC reads ([ADR-0053](./docs/adr/0053-lock-free-mvcc-reads.md)). `v0.19.0` — *hybrid everywhere + fast ingest* ([ADR-0045](./docs/adr/0045-hybrid-everywhere-and-fast-ingest.md)): a **derived sparse inverted index** replaces the per-query store scan on the sparse half of hybrid search (built from the store, maintained incrementally on upsert/delete, with a store-scan fallback — no on-disk change, crash gate untouched); **hybrid search now reaches every surface** — a gRPC `HybridSearch` RPC, an MCP `hybrid_search` tool, and a TypeScript `hybridSearch` method join the REST endpoint and the Python SDK; and a **bulk-ingest endpoint** (`POST /v1/collections/{name}/points:bulk`) commits a large batch with one WAL fsync and a single index-build pass (the path that addresses the benchmark's build-time column), bounded by `QUIVER_MAX_BULK_BATCH_SIZE`. `v0.18.1` is a release-packaging patch: the release workflow now builds and publishes the Windows binary automatically on every tagged release (it was previously uploaded by hand, so `v0.18.0` shipped with no assets and `quiver update` could not fetch `quiver-windows-x86_64.exe` — see [ADR-0044](./docs/adr/0044-automated-release-assets.md)). `v0.18.0` is the post-launch correctness, benchmark, and RAG pass (opened by a full [state-of-Quiver assessment](./docs/analysis/state-of-quiver-v0.17.md)): **query cost limits** enforced at the op layer (closing an authenticated-DoS and the one docs/code mismatch); a real **SIFT1M / GIST1M** multi-DB benchmark — Quiver is **second only to FAISS** on SIFT1M (QPS + tail latency at recall ≥ 0.95) and **matches FAISS on GIST recall** (tables below); **hybrid dense + sparse search with RRF fusion** (engine + REST + Python SDK); an **async Python SDK**, a **Haystack** `DocumentStore` (joining LangChain + LlamaIndex), a `rerank` helper, an MCP `collection_info` tool, and **complete RAG / agentic / tuning usage guides** with a runnable example; plus a cockpit that shows the **release version beside the wordmark** with richer vitals. `v0.17.0` delivered two Phase 5 hardening items: a **35× build-time speedup** (batch WAL sync — 65.4s → 1.86s for 10k SIFTSMALL vectors, from dead last to middle-of-field) and a **one-command install** (`scripts/install.sh` / `install.ps1`, SHA-256 verified pre-built binaries, `quiver update` self-update subcommand). Full benchmark comparison against 7 competitors (FAISS, Qdrant, Milvus Lite, Chroma, pgvector, LanceDB, Weaviate) in [`docs/benchmarks/results/comparison-v0.17.0`](./docs/benchmarks/results/comparison-v0.17.0/comparison-v0.17.0.md). Phase 1 (`v0.1.0`) shipped the single-node core — an encrypted, crash-safe storage engine, HNSW, SIMD kernels, REST/gRPC, the TUI, and the Python SDK. Phase 2 (`v0.2.0`) delivered memory frugality: the disk-resident DiskANN/Vamana and IVF indexes with product/scalar/binary quantization, a row-addressed storage engine (stride-addressed vector columns, paged payload heaps, roaring tombstones, compaction, secondary indexes), **hybrid filtered search**, the TypeScript SDK, the MCP server, and LangChain/LlamaIndex adapters. Phase 3 (`v0.3.0`) added security depth and cockpit polish: client-side payload encryption, RBAC with scoped API keys and optional mTLS, an append-only audit log, per-collection-DEK encryption with crypto-shredding, master-key-file secret handling, the 2-D **constellation view**, and `cargo-fuzz` targets. Phase 4 ships the advanced backlog incrementally: `v0.4.0` added **incremental in-place index updates** (SpFresh/LIRE for IVF) and **migration importers** (`quiver admin import` for Qdrant/Chroma/pgvector); `v0.5.0` added **HNSW incremental delete**, a neighbor-bounded IVF reassignment, a unified secure database-open path across the server/MCP/CLI, and the design for a durable on-disk incremental index. `v0.6.0` made that durable index real — the IVF index now loads on open (snapshot + WAL-tail replay) instead of an `O(N)` rebuild, crash-gated — and added a **live Qdrant migration connector** (`quiver admin import --qdrant-url`). `v0.7.0` adds **multi-vector / late-interaction (ColBERT) retrieval**: a collection can store each document as a set of token vectors and rank documents by **MaxSim** — reusing the row store (so the crash gate is untouched) and the IVF+PQ frugality path — reachable from the embeddable database, REST/gRPC, the MCP server, and the SDKs. `v0.8.0` extends migration to **live Chroma and Postgres connectors** (`quiver admin import --chroma-url` / `--postgres-url`), so all three supported sources can import directly from a running instance — no export step. `v0.9.0` adds **asynchronous leader-follower read replicas** (point a follower at a leader with `QUIVER_LEADER_URL`) — scaling reads and giving warm standbys without consensus or failover. `v0.10.0` adds an **experimental, opt-in DCPE vector-encryption mode** (`vector_encryption="dcpe"`): a client encrypts embeddings with a published distance-comparison-preserving scheme so an untrusted server can rank ciphertexts by approximate L2 distance without ever holding the plaintext vectors or the key — honestly labelled, since it is L2-only, not semantically secure, and leaks the approximate distance ordering by design. `v0.11.0` adds a **semantically secure** client-side mode (`vector_encryption="client_side"`): the server stores only XChaCha20-Poly1305 ciphertext plus a zero placeholder, learns nothing about the vectors (genuinely IND-CPA), and does no ranking — so the client fetches the (optionally pre-filtered) set and ranks locally, with native Rust/Python/TypeScript ciphers validated by a bit-exact cross-language test. `v0.12.0` is a documentation & packaging fix — it corrects the install guidance (build from source; the `quiver-cli` name on crates.io is an unrelated project, and publishing the SDKs is a roadmap item) and the README rendering, with no functional change. `v0.13.0` brings **incremental updates to the last index family that still rebuilt on every write**: the Vamana and disk-resident DiskVamana graphs now use **FreshDiskANN's StreamingMerge** — a read-only base graph plus a small in-memory delta graph and an `O(1)` deletion set, searched together and consolidated by a derived rebuild past a churn threshold — so graph writes become size-independent while the index stays derived and the `kill -9` crash gate is untouched. `v0.14.0` takes two of the multi-vector / ColBERT follow-ups: document upsert/delete now maintain the token-pool index **incrementally** (no full rebuild), and an opt-in `colbert` index adds **ColBERTv2 residual compression + PLAID centroid pruning** for `multivector` collections — both derived, so the crash gate stays untouched; native variable-stride document rows are deferred pending a reference-hardware locality measurement. `v0.15.0` ships the documentation polish before launch: an **mdBook documentation site** under `apps/docs` (concepts → quickstart → self-hosting → features → API/SDKs → security → architecture), a verified clean-clone quickstart, a **native TypeScript DCPE cipher** (closing the last SDK gap), and the two deferred **DCPE Scale-And-Perturb hardening** steps — a key-derived component shuffle and an ordering-preserving global normalisation — shipped as a breaking cipher v2 across Rust/Python/TypeScript with a cross-language known-answer test, honest about the one limit it cannot cross (per-axis whitening would break searchable ordering). `v0.16.0` makes the **terminal cockpit** the headline: a coherent **Bronze Quiver** retro brand (bronze chrome, leather borders, parchment text, a verdigris accent, on oak-black), a **logo whose V is a 3-D arrowhead**, and a vocabulary of minimalist retro decorations — framed panels, a database drum icon, a collections table with per-row load bars, a points-trend sparkline, a relationship tree, status badges, and a severity-tagged activity log — so an operator grasps the data, its structure, and what is happening at a glance; the view code is decoupled behind a render-to-buffer API, and a workspace-isolated generator renders each screen to a committed PNG (`just tui-shots`) for the README and docs from the *real* render, so the screenshots never go stale. Every performance/memory claim in this README is backed by a reproducible benchmark on documented reference hardware — until those numbers are recorded, that table stays empty rather than guess.

## Why Quiver

Native-Rust vector databases already exist; Quiver is not trying to out-scale Milvus or out-feature Qdrant. Its defensible edge is the **combination** of three things, executed well:

- **Security-first, by default** — encryption-at-rest is on out of the box, sealing every durable byte (segments, manifest, **and** the write-ahead log) with XChaCha20-Poly1305; payloads can be client-side-encrypted so the server never sees them; API-key scopes, RBAC, tenant isolation, audit, and crypto-shredding. Only audited cryptography (RustCrypto AEAD/KDF + `rustls`) — never a home-grown primitive. The parsers that touch untrusted input (the search-filter wire format and the on-disk page/WAL decoders) are [fuzzed](./docs/security/fuzzing.md).
- **Memory frugality** — a disk-resident graph index (DiskANN/Vamana) plus quantization (product / scalar / binary) serve large datasets from a laptop's RAM budget. The headline metric is **memory at a fixed recall**.
- **Developer experience** — a single static binary; embeddable *and* server modes; a `ratatui` cockpit with a 2-D constellation view of the vector space; idiomatic Python/TypeScript SDKs; an MCP server so agents can drive it.

We say plainly what we do **not** do: client-side payload encryption protects *payloads, not vectors* (the experimental, opt-in [DCPE mode](./docs/security/dcpe.md) addresses vectors — a published scheme that, by design, leaks the approximate distance-comparison relation and is not semantically secure); billion-scale needs a server, while a laptop comfortably serves tens-to-hundreds of millions; there is no homomorphic search in core. See the honest [threat model](./docs/security/threat-model.md).

> *The name.* A quiver holds arrows, and an arrow is a vector — apt for a database of them. And in mathematics a *quiver* is a directed graph, which is exactly what an HNSW or Vamana index is. The cockpit wears that identity in **bronze** — the colour of a quiver, with the logo's V drawn as a 3-D arrowhead.

## The cockpit

A retro terminal cockpit ships in the box (`quiver tui`): a live dashboard in the **Bronze Quiver** theme — connection health and an `ONLINE`/`OFFLINE` badge, a collections table with per-collection load bars, points-trend and ingest-rate sparklines, the relationship view of the selected collection, and a severity-tagged activity log.

![The Quiver cockpit dashboard](docs/assets/cockpit/dashboard.png)

Press `v`/`enter` on a collection for the **constellation view** — a 2-D projection of its vector space with the query's nearest neighbour highlighted and an interactive cursor that re-queries around any point:

![The Quiver constellation view](docs/assets/cockpit/constellation.png)

Press `/` for the **query runner** — type a query, run a server-side embed-and-search (ADR-0047), inspect any result's payload, and recall recent searches:

![The Quiver query runner](docs/assets/cockpit/search.png)

`?` opens a keybinding overlay and `Ctrl-t` toggles a cool **Slate** palette. The whole UI renders to a buffer behind a render-to-buffer API, so every screen is unit-tested with ratatui's `TestBackend` and the screenshots are generated from the *real* render of seeded demo data with `just tui-shots` (a dev-only, workspace-isolated tool) — they regenerate in one command and never go stale ([ADR-0036](./docs/adr/0036-retro-cockpit-design-system.md), [ADR-0060](./docs/adr/0060-interactive-tui-cockpit.md)).

## 📖 Field guide — *Quiver, Explained*

A complete, **beginner-to-expert** walkthrough of how Quiver works — embeddings and approximate-nearest-neighbour search from first principles, the engine block by block (SIMD kernels, HNSW / Vamana-DiskANN / IVF, quantization, hybrid search), durability and the `kill -9` crash gate, the security model (envelope encryption, crypto-shredding, encrypted vectors), and the benchmark results with the verdict on Quiver's value. Written for people who know nothing about vector databases *and* engineers who want the depth — illustrated with diagrams and charts, typeset in the Bronze Quiver theme.

> **→ [Read the PDF field guide](./docs/quiver-explained.pdf)** &nbsp;·&nbsp; 33 pages, fully illustrated &nbsp;·&nbsp; also available as [Markdown](./docs/quiver-explained.md)

## Architecture

A Cargo workspace: a from-scratch storage engine, index structures, SIMD distance kernels, and query planner, with a thin gRPC/REST shell and a TUI client. One binary runs the server, the cockpit, and the MCP server.

→ [System context](./docs/architecture/c4-context.md) · [Container view](./docs/architecture/c4-container.md) · [Overview & crate map](./docs/architecture/overview.md) · [ADRs](./docs/adr) · [State of Quiver (assessment)](./docs/analysis/state-of-quiver-v0.17.md)

## Quickstart

> **Full documentation** lives in the [docs site](./apps/docs) (an mdBook; build it with `just docs`, or read the chapters under [`apps/docs/src`](./apps/docs/src)) — concepts, self-hosting, every feature, the API/MCP/SDK references, the security docs, and an architecture deep dive.

**Install (Linux / macOS) — one command, no Rust toolchain required:**

```bash
curl -fsSL https://raw.githubusercontent.com/achref-soua/quiver/main/scripts/install.sh | sh
```

**Windows (PowerShell 5.1+):**

```powershell
irm https://raw.githubusercontent.com/achref-soua/quiver/main/scripts/install.ps1 | iex
```

<img src="docs/assets/cockpit/installer.png" alt="Quiver installer output — retro logo, download progress, next steps" width="540">

Both scripts detect your OS and architecture, download the pre-built binary for
your platform from the [latest GitHub Release](https://github.com/achref-soua/quiver/releases/latest),
verify its SHA-256 checksum before touching your disk, and install to `~/.local/bin`
(Linux/macOS) or `%LOCALAPPDATA%\quiver\bin` (Windows). On Linux the installer also
creates a `.desktop` entry and app-launcher icon. On macOS it creates a `Quiver.app`
bundle with the custom icon so you can pin it to the Dock. The Windows binary has the
icon embedded natively. To pin a specific version, set `QUIVER_VERSION=0.17.0` before running.

Once installed, keep Quiver up to date with:

```bash
quiver update           # downloads, verifies, and atomically replaces the binary
quiver update --check   # just check if a newer version exists
```

**Zero-config first run:**

```bash
quiver demo
```

<img src="docs/assets/cockpit/demo-start.png" alt="quiver demo output — seeds vectors, starts server, opens cockpit" width="540">

Seeds 1 000 synthetic vectors, starts the REST server on `:7333`, and opens the retro
cockpit — no config files, no env vars, no external downloads.

**Full server quick start:**

```bash
quiver serve            # gRPC + REST on :6333, encrypted by default
quiver tui              # the retro cockpit
quiver mcp              # MCP server (stdio) so AI agents can drive Quiver
```

**Build from source** (requires rustup stable + `just` + `uv`):

```bash
git clone https://github.com/achref-soua/quiver
cd quiver
just demo             # build, start an encrypted server, seed a demo collection
# then, in another terminal:
quiver tui --api-key quiver-demo-key   # the retro cockpit
```

`just demo` brings up a server with **encryption-at-rest on**, seeds a small
collection through the Python SDK, and prints how to open the cockpit. In the
cockpit, press `v` (or `enter`) on a collection to open the **constellation
view** — a 2-D random-projection scatter of its vector space with the query's
nearest neighbour highlighted; move the cursor and press `enter` to re-query
around any point. _(The recorded cockpit cast lands in `docs/assets/`; produce
it on a real terminal with `scripts/record-cockpit-cast.sh`.)_ To build and
exercise the workspace directly:

```bash
just build            # compile the workspace
just verify           # the full local quality gate (lint · test · doc · deny · audit)
cargo run -p quiverdb-cli -- --help
```

> **Heads-up:** the `quiver-cli` crate currently on crates.io is an unrelated
> third-party project — use the install script above or build from source.

The [MCP server](./docs/mcp.md) exposes `create_collection`, `upsert`, `search`,
`get`, `delete`, and the multi-vector `upsert_document` / `search_multi_vector` /
`delete_document` tools over JSON-RPC stdio, operating an encrypted in-process
database.

## Command reference

All developer tasks run through [`just`](./justfile):

| Command | What it does |
|---|---|
| `just build` | build the workspace (all targets) |
| `just test` | run the test suite |
| `just lint` | `cargo fmt --check` + `clippy -D warnings` |
| `just verify` | **the gate** — lint · test · doc · deny · audit |
| `just test-py` | Python SDK test suite (via `uv`) |
| `just run` / `just tui` | run the server / the cockpit |
| `just demo` | encrypted server + seeded demo collection |
| `just bench *ARGS` | run the benchmark harness (e.g. `just bench --synthetic`) |
| `just coverage` | HTML coverage report |
| `just docker` | build the container image |

The `ci` (fmt · clippy · test · doc) and `security` (deny · audit · gitleaks) workflows under [`.github/workflows`](.github/workflows) run automatically on every pull request and on pushes to `main`/`develop`; the heavier `build` workflow stays manual (`workflow_dispatch`). Local `just verify` runs the same steps as the fast pre-commit gate, so the two never drift ([ADR-0015](./docs/adr/0015-ci-policy.md)).

## SDK & benchmarks

The **Python SDK** lives in [`sdks/python`](./sdks/python) (`pip install ./sdks/python`):

```python
from quiver import Client, Point

with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("items", dim=3, metric="cosine")
    q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
    hits = q.search("items", [0.1, 0.2, 0.3], k=5)
```

The **TypeScript SDK** lives in [`sdks/typescript`](./sdks/typescript) (`pnpm add ./sdks/typescript`), dependency-free over the global `fetch`, and can pick the memory-frugal disk index:

```ts
import { Client } from "quiver-client";

const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });
await q.createCollection("items", 3, { metric: "cosine", index: "disk_vamana", pqSubspaces: 1 });
await q.upsert("items", [{ id: "a", vector: [0.1, 0.2, 0.3], payload: { tag: "x" } }]);
const hits = await q.search("items", [0.1, 0.2, 0.3], { k: 5 });
```

A **LangChain** `VectorStore` adapter ships in `quiver.langchain` (`pip install "./sdks/python[langchain]"`), and a **LlamaIndex** `VectorStore` in `quiver.llamaindex` (`pip install "./sdks/python[llamaindex]"`) — so any Quiver index, including the memory-frugal disk path, backs a LangChain or LlamaIndex retriever. The LlamaIndex adapter maps `MetadataFilters` onto Quiver's hybrid pre-filter. A synchronous `Client` and an async `AsyncClient` share one contract, with batched-upsert/scan/delete-by-filter helpers for ingestion and erasure.

**Using Quiver in RAG / agents.** End-to-end guides — [RAG](./apps/docs/src/guides/rag.md) (chunk → embed → filtered search → rerank → answer), [agentic patterns over MCP](./apps/docs/src/guides/agentic.md), and [tuning for RAG](./apps/docs/src/guides/tuning.md) (index/quantizer/recall-RAM) — plus a runnable [`examples/rag/quickstart.py`](./examples/rag/quickstart.py) that needs no API key.

**Client-side payload encryption** (ADR-0012): seal payload fields with a key the server never sees, so it stores and returns only ciphertext, while cleartext sibling fields stay server-filterable. The `PayloadCipher` helper ships in both SDKs (`quiver.encryption` / `quiver-client/encryption`) and a Rust reference (`quiver_crypto::payload`), sharing one XChaCha20-Poly1305 envelope byte-for-byte. The trust boundary is honest — it protects payloads, not vectors — and proven by a test that runs a server with at-rest encryption off and shows the sealed field never appears in plaintext over the API or on disk.

An `ann-benchmarks`-style harness lives in [`bench/`](./bench). On **SIFT1M** (1M × 128, L2), in-memory HNSW (`M=16`, `efC=200`), Quiver's own recall ↔ throughput ↔ latency curve:

| `ef_search` | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| **recall@10** | 0.793 | 0.895 | 0.958 | 0.986 | 0.995 |
| **QPS** (1 thread) | 1539 | 1424 | 1222 | 955 | 701 |
| **p95 latency** (ms) | 0.8 | 0.8 | 1.0 | 1.3 | 1.7 |

**Head-to-head on SIFT1M**, every system on the *same* box (i7-12700H · 20 threads · 15.5 GB), peak single-thread QPS at **recall@10 ≥ 0.95** (full method, sweeps, and the wins/losses matrix: [`comparison-v0.20.0`](./docs/benchmarks/results/comparison-v0.20.0/comparison-v0.20.0.md)):

| System | recall@10 | QPS (1T) | p95 (ms) | RSS (MB) | build |
|---|---:|---:|---:|---:|---:|
| FAISS 1.14 | 0.968 | **3842** | 0.4 | 1234 ¹ | 82 s |
| **Quiver v0.20** | 0.958 | **1222** | **1.0** | 2069 | 581 s ² |
| Chroma 1.5 | 0.977 | 1009 | 1.1 | 3752 ¹ | 153 s |
| Weaviate 1.27 | 0.983 | 663 | 1.7 | 2218 | 38 min |
| Milvus 2.5 (server) | 0.986 | 649 | 1.9 | 2075 | 26 s |
| Qdrant 1.13 | 0.974 | 358 | 4.5 | **258** ³ | 98 s |
| pgvector 0.7 | 0.980 | 118 | 11.8 | 1291 | 132 s |
| LanceDB 0.33 | 0.557 ⁴ | 219 | 5.3 | 2475 ¹ | 15 s |

Quiver is **second only to FAISS** on both throughput and tail latency at this recall bar, with recall on par with the field — and on the v0.20.0 engine its single-thread QPS rose ~40% over v0.18.0 (870 → 1222 at recall ≥ 0.95) while p95 dropped from 1.5 ms to 1.0 ms.

The honesty that makes the table trustworthy: this is an **in-memory HNSW** comparison for *every* system, so RSS here is full-vectors-in-RAM — Quiver's memory-frugality wedge is its **disk-resident path** (only PQ codes resident; ~32× less RAM, see [`disk-path.md`](./docs/benchmarks/results/disk-path.md)), **not** this table. ¹ FAISS/Chroma/LanceDB run in-process so their RSS includes the Python harness + the resident 512 MB dataset (inflated; only Quiver/Milvus/Qdrant/Weaviate report the isolated DB). ² Quiver's "build" is now the **bulk-ingest** path (`POST …/points:bulk`, [ADR-0045](./docs/adr/0045-hybrid-everywhere-and-fast-ingest.md)) — one WAL fsync per request plus a single deferred index pass, with the first query forcing the rebuild so the number is the honest *time-until-queryable* (581 s, down from v0.18.0's 854 s REST-upload path). In-process FAISS still builds fastest as it skips the network entirely. ³ Qdrant mmaps vectors to disk by default. ⁴ LanceDB's IVF-PQ config doesn't reach 0.95 recall in this sweep (shown at its best). Numbers are **dev-box, indicative** — comparative standings on the identical box are real (per the [methodology](./docs/benchmarks/methodology.md)); **absolute** RSS, saturated multi-thread QPS, and the 10M disk path are reference-hardware-pending; we never fabricate. Milvus is benchmarked as the **server** (Docker), not the in-process Lite build.

**GIST1M** (1M × 960, L2) is the harder, higher-dimensional test. Same box, each system at its most efficient config reaching recall@10 ≥ 0.95, or its best point at `ef_search ≤ 256` (960-d needs a wide beam, so most plateau below 0.95 in this sweep):

| System | recall@10 | QPS (1T) | p95 (ms) | RSS (MB) | ef/nprobe |
|---|---:|---:|---:|---:|---:|
| **Quiver v0.20** | **0.923** | 268 | 4.4 | 10117 | 256 |
| FAISS 1.14 | 0.919 | **471** | 2.7 | 7526 ¹ | 256 |
| Chroma 1.5 | 0.790 | 577 | 2.1 | 8156 ¹ | 16 ⁵ |
| Weaviate 1.27 | 0.828 | 418 | 2.8 | 8880 | 64 ⁵ |
| Qdrant 1.13 | 0.955 | 185 | 6.3 | **391** ³ | 128 |
| Milvus 2.5 (server) | 0.961 | 53 | 29.4 | 6821 | 64 |
| pgvector 0.7 | 0.980 | 8 | 194 | 4393 | 64 |

On 960-d, **Quiver matches FAISS on recall (0.923 vs 0.919)** and on the v0.20.0 engine is markedly faster than v0.18.0 at the same recall (182 → 268 QPS, p95 7.7 → 4.4 ms). The three systems that clear 0.95 here — Qdrant (185 QPS), Milvus (53), pgvector (8) — pay heavily for it in throughput and tail latency. ⁵ Chroma and Weaviate plateau well below 0.95 — their `ef_search` does not widen recall in this config. **LanceDB did not complete** GIST1M: building a 960-d/1M IVF-PQ index in-process exhausts memory (an honest DNF even with 27 GB of swap, not a fabricated row). Same RSS caveats as above (¹ in-process = inflated; ³ Qdrant disk-backed; Quiver's in-memory HNSW holds vectors in RAM — the disk path is the memory wedge). Full sweeps + the wins/losses matrix: [`comparison-v0.20.0`](./docs/benchmarks/results/comparison-v0.20.0/comparison-v0.20.0.md).

**New in v0.22.0 — four measurement dimensions** ([ADR-0061](./docs/adr/0061-benchmark-dimensions-v0.22.0.md)) on SIFT1M, every number traced to a committed CSV in [`comparison-v0.22.0`](./docs/benchmarks/results/comparison-v0.22.0/comparison-v0.22.0.md) (dev-box · indicative). The headline is **saturated concurrency** — the payoff of the v0.21.0 reader–writer lock and the v0.22.0 off-lock rebuild: QPS under 8 client threads (NT) vs one (1T), per `ef_search`:

| ef_search | 16 | 32 | 64 | 128 | 256 |
|---|---:|---:|---:|---:|---:|
| recall@10 | 0.793 | 0.895 | 0.958 | 0.986 | **0.995** |
| QPS (1T) | 1131 | 1001 | 855 | 673 | 506 |
| QPS (8T) | 949 | 968 | 928 | 938 | 892 |
| **speed-up** | 0.84× | 0.97× | 1.08× | 1.39× | **1.76×** |

Read honestly: a single-process Python client (GIL + one HTTP socket) is itself a ceiling, so *light* queries are client-bound (NT ≤ 1T); the server-side win shows on *heavier* queries, where parallel readers pull ahead — up to **1.76× at ef=256**, not a fabricated “8×”. The run also reports **recall at depth 1/10/100** (recall@100 needs a wider beam: 0.918 → 0.983 as ef grows), a **quantization tradeoff** (in-memory HNSW recall@100 0.944 vs disk-Vamana + PQ16 0.709 — PQ trades the deep tail; absolute serving-RAM stays reference-hardware-pending), and a **filtered-selectivity** sweep (the planner's pre-filter/post-filter recall valley: 1.0 at 1%, 0.62 at 5%, recovering to 0.99). A walkthrough of all four, with figures, is in the ["Quiver, Explained" field guide](./docs/quiver-explained.pdf) (Part 7).

The per-collection **recall ↔ latency ↔ memory** knobs — quantizers (scalar/product/binary), the disk-resident DiskANN path, and IVF — are documented with a tradeoff table in [`docs/benchmarks/quantization-tradeoffs.md`](./docs/benchmarks/quantization-tradeoffs.md).

Every index supports **incremental updates**, so streaming workloads avoid an `O(N)` rebuild on each write. The **IVF** index applies inserts, in-place updates, and deletes to the live index with SpFresh-style LIRE rebalancing (cell split/merge) ([ADR-0023](./docs/adr/0023-incremental-in-place-updates.md)); **HNSW** soft-deletes in `O(1)` with an amortized rebuild ([ADR-0026](./docs/adr/0026-hnsw-incremental-delete.md)); and the **Vamana / disk-resident graph** family uses **FreshDiskANN's StreamingMerge** — a read-only base graph plus a small in-memory delta graph and an `O(1)` deletion set, consolidated by a derived rebuild past a churn threshold ([ADR-0033](./docs/adr/0033-graph-incremental-freshdiskann.md)). All indexes stay derived and the disk artifact keeps its write-once contract, so the `kill -9` crash gate is untouched.

The **disk-resident path** is the memory-frugality wedge. On SIFTSMALL (128-d), it serves recall@10 up to **1.000** while holding only PQ codes in RAM — a **32× smaller RAM-resident footprint** than full-precision vectors (the graph and vectors live in the encrypted on-disk index). That reduction is exact arithmetic and scales (e.g. a 10M × 768-d collection: ~1 GB resident vs ~31 GB). The head-to-head **RSS vs Qdrant/LanceDB** is reference-hardware-pending. Numbers and method: [`docs/benchmarks/results/disk-path.md`](./docs/benchmarks/results/disk-path.md).

**Multi-vector / late interaction (ColBERT).** Create a collection `multivector` and each document is stored as a *set* of token vectors and ranked by **MaxSim** — for each query token, its best-matching document token, summed. Quiver models a document as a group of ordinary rows over the same row-addressed store, so there is **no on-disk format change and the `kill -9` crash gate is untouched**; the token pool is the set the ANN index serves (candidate generation), then candidates are re-ranked by exact MaxSim with an optional payload filter. A ColBERT corpus is exactly the large, low-dimensional pool the IVF+PQ / disk path was built to compress, so late interaction showcases the memory-frugality wedge. Reachable from the embeddable database, REST + gRPC, the MCP server, and the Python/TypeScript SDKs ([ADR-0028](./docs/adr/0028-multi-vector-late-interaction.md)). `v0.14.0` adds two follow-ups ([ADR-0034](./docs/adr/0034-multivector-followups.md)): document upsert/delete now maintain the token-pool index **incrementally** (no full rebuild, so a document write is size-independent), and an opt-in `colbert` index applies **ColBERTv2 residual compression + PLAID centroid pruning** — coarse centroids plus per-token quantized residual codes in RAM, with the exact token vectors on the encrypted store for the re-rank. Both stay derived (rebuilt on open), so the crash gate is untouched; native variable-stride document rows are deferred pending a reference-hardware locality measurement.

## Migrating from another vector database

Move an existing collection out of **Qdrant**, **Chroma**, or **pgvector** with one command — from an export file, or **live** from a running instance (no export step):

```bash
# from an export file
quiver admin import --source qdrant --input qdrant.jsonl \
  --collection my_collection --data-dir ./data --metric cosine

# or live, straight from a running source
quiver admin import --source chroma --chroma-url http://localhost:8000 \
  --collection docs --data-dir ./data --metric cosine
quiver admin import --source pgvector \
  --postgres-url postgresql://user:pass@localhost/db \
  --table items --collection items --data-dir ./data --metric l2
```

The importer preserves ids, vectors, and payloads, optionally declares `--filterable path:type` fields for hybrid search, and writes the same encrypted format the server reads — so the result is an ordinary Quiver store you can `quiver serve` immediately. Live connectors for all three sources share the offline path's normalization ([ADR-0027](./docs/adr/0027-live-migration-connectors.md), [ADR-0029](./docs/adr/0029-live-chroma-postgres-connectors.md)). Per-source recipes and the full option reference are in [`docs/migration.md`](./docs/migration.md) ([ADR-0024](./docs/adr/0024-migration-importers.md)).

## Replication

Run **asynchronous read replicas** (ADR-0030): point a follower at a leader with `QUIVER_LEADER_URL` and it continuously applies the leader's committed operations and serves reads, lagging by the replication delay. Followers refuse writes; the leader exposes an admin-scoped `Replicate` stream that ships a logical snapshot, then the live commit tail. This scales reads and gives warm standbys **without** consensus or failover — single-node stays the primary topology, and this is a clearly-labelled advanced feature.

```bash
QUIVER_LEADER_URL=http://leader-host:6334 QUIVER_LEADER_API_KEY=<admin key> quiver serve
```

See [`docs/replication.md`](./docs/replication.md) for the topology, guarantees, and limits.

## Encrypted vector search

Search your embeddings on a server you don't fully trust, choosing per collection (`vector_encryption`) where you sit on the confidentiality/performance spectrum — because no scheme gives fast server-side ranking, zero leakage, and practical performance all at once.

**DCPE (`vector_encryption: "dcpe"`, experimental).** The client encrypts vectors with **distance-comparison-preserving encryption** — the published [Scale-And-Perturb scheme](https://eprint.iacr.org/2021/1666), built only from audited RustCrypto primitives — so the server can rank ciphertexts by approximate L2 distance **without ever holding the plaintext vectors or the key** (ADR-0031). It is **not semantically secure**: L2-only, and it **leaks the approximate distance-comparison relation by design** (that is how the server ranks), so it carries real, documented caveats and is broken by known-plaintext or strong-prior adversaries. The **v2 cipher** ([ADR-0035](./docs/adr/0035-docs-site-and-dcpe-hardening.md)) adds the paper's two hardening steps — a key-derived component **shuffle** (an exact L2 isometry) and an ordering-preserving global **normalisation** — and ships native ciphers in **Rust, Python, and TypeScript**, validated against each other by a cross-language known-answer test. Read [`docs/security/dcpe.md`](./docs/security/dcpe.md) before using it.

**Client-side opaque vectors (`vector_encryption: "client_side"`, semantically secure).** The server stores only XChaCha20-Poly1305 ciphertext (no new cryptography — the same audited AEAD as at-rest) plus a zero placeholder, does **no** distance math, and learns **nothing** about the vectors — no coordinates, no distances, no geometry (genuinely IND-CPA). The honest cost: the server doesn't rank, so the client fetches the (optionally pre-filtered) set and ranks locally — best for small/medium or server-pre-filtered collections. Ships as a native `VectorCipher` in Rust/Python/TypeScript with a bit-exact cross-language test, plus a `search`-style helper that hides the fetch-and-rank round-trip (ADR-0032). Read [`docs/security/client-side-vectors.md`](./docs/security/client-side-vectors.md).

Both modes are opt-in and off by default, and **complement** encryption-at-rest rather than replacing it.

## Configuration

Every option is an environment variable with a secure default; see [`.env.example`](./.env.example) and [ADR-0013](./docs/adr/0013-config-and-secure-defaults.md). Encryption-at-rest is on by default: the server requires a 256-bit key in `QUIVER_ENCRYPTION_KEY` (generate one with `openssl rand -hex 32`) unless `QUIVER_INSECURE=true`, and seals segments, the manifest, and the WAL alike. That key is a **master key** that wraps a per-collection data-encryption key (envelope encryption, [ADR-0010](./docs/adr/0010-crypto-envelope-aead.md)), so dropping a collection **crypto-shreds** it — its key is destroyed and its data becomes unrecoverable, even from a backup ([details](./docs/security/crypto.md)). TLS is required for any non-loopback bind.

**Access control (ADR-0011):** authentication is by API key and authorization is **default-deny RBAC**. A bare `QUIVER_API_KEYS` secret is an all-collections admin key; for least privilege, define scoped keys in `quiver.toml` with a `role` (`read` ⊆ `write` ⊆ `admin`) and a `collections` scope (exact names or a trailing-`*` prefix, e.g. `acme.*`, for per-namespace isolation). A key may only perform its role's actions within its scope — over-scope and cross-namespace access return `403`, and listing hides collections outside the scope. For an extra factor, set `QUIVER_TLS_CLIENT_CA` to require **mutual TLS**: both transports then demand a client certificate chaining to that CA. Set `QUIVER_AUDIT_LOG` to record every mutating/administrative operation and every denial to an append-only [audit log](./docs/security/audit.md) — the acting key, the action, the resource, and the outcome, **never the secret**.

## Project

- **Documentation site** — [`apps/docs`](./apps/docs) (mdBook; `just docs`)
- **Roadmap & Definitions of Done** — [`docs/roadmap.md`](./docs/roadmap.md)
- **Changelog** — [`CHANGELOG.md`](./CHANGELOG.md)
- **Security policy** — [`SECURITY.md`](./SECURITY.md) · **Threat model** — [`docs/security/threat-model.md`](./docs/security/threat-model.md)
- **Contributing** — [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- **License** — [AGPL-3.0-only](./LICENSE)
