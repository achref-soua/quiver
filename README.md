<div align="center">

<img src="docs/assets/cockpit/logo.png" alt="QUIVER — the V is a 3-D arrowhead" width="460">

# Quiver

**The security-first vector database.** Client-side-encryptable, memory-frugal approximate-nearest-neighbour search that runs on a laptop — with a retro terminal cockpit.

[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-stable-orange)](./rust-toolchain.toml)
[![CI](https://img.shields.io/badge/CI-manual%20dispatch-informational)](.github/workflows)
[![release](https://img.shields.io/badge/release-v0.16.0-FFB000)](https://github.com/achref-soua/quiver/releases)
[![status](https://img.shields.io/badge/status-v0.17.0%20·%20phase%205-FFB000)](./docs/roadmap.md)
[![stars](https://img.shields.io/badge/Star_on-GitHub-FFB000?logo=github)](https://github.com/achref-soua/quiver/stargazers)

</div>

> **Status: `v0.17.0` released · launch-hardening complete.** `v0.17.0` delivers two Phase 5 hardening items: a **35× build-time speedup** (batch WAL sync — 65.4s → 1.86s for 10k SIFTSMALL vectors, from dead last to middle-of-field) and a **one-command install** (`scripts/install.sh` / `install.ps1`, SHA-256 verified pre-built binaries, `quiver update` self-update subcommand). Full benchmark comparison against 7 competitors (FAISS, Qdrant, Milvus Lite, Chroma, pgvector, LanceDB, Weaviate) in [`docs/benchmarks/results/comparison-v0.17.0`](./docs/benchmarks/results/comparison-v0.17.0/comparison-v0.17.0.md). Phase 1 (`v0.1.0`) shipped the single-node core — an encrypted, crash-safe storage engine, HNSW, SIMD kernels, REST/gRPC, the TUI, and the Python SDK. Phase 2 (`v0.2.0`) delivered memory frugality: the disk-resident DiskANN/Vamana and IVF indexes with product/scalar/binary quantization, a row-addressed storage engine (stride-addressed vector columns, paged payload heaps, roaring tombstones, compaction, secondary indexes), **hybrid filtered search**, the TypeScript SDK, the MCP server, and LangChain/LlamaIndex adapters. Phase 3 (`v0.3.0`) added security depth and cockpit polish: client-side payload encryption, RBAC with scoped API keys and optional mTLS, an append-only audit log, per-collection-DEK encryption with crypto-shredding, master-key-file secret handling, the 2-D **constellation view**, and `cargo-fuzz` targets. Phase 4 ships the advanced backlog incrementally: `v0.4.0` added **incremental in-place index updates** (SpFresh/LIRE for IVF) and **migration importers** (`quiver admin import` for Qdrant/Chroma/pgvector); `v0.5.0` added **HNSW incremental delete**, a neighbor-bounded IVF reassignment, a unified secure database-open path across the server/MCP/CLI, and the design for a durable on-disk incremental index. `v0.6.0` made that durable index real — the IVF index now loads on open (snapshot + WAL-tail replay) instead of an `O(N)` rebuild, crash-gated — and added a **live Qdrant migration connector** (`quiver admin import --qdrant-url`). `v0.7.0` adds **multi-vector / late-interaction (ColBERT) retrieval**: a collection can store each document as a set of token vectors and rank documents by **MaxSim** — reusing the row store (so the crash gate is untouched) and the IVF+PQ frugality path — reachable from the embeddable database, REST/gRPC, the MCP server, and the SDKs. `v0.8.0` extends migration to **live Chroma and Postgres connectors** (`quiver admin import --chroma-url` / `--postgres-url`), so all three supported sources can import directly from a running instance — no export step. `v0.9.0` adds **asynchronous leader-follower read replicas** (point a follower at a leader with `QUIVER_LEADER_URL`) — scaling reads and giving warm standbys without consensus or failover. `v0.10.0` adds an **experimental, opt-in DCPE vector-encryption mode** (`vector_encryption="dcpe"`): a client encrypts embeddings with a published distance-comparison-preserving scheme so an untrusted server can rank ciphertexts by approximate L2 distance without ever holding the plaintext vectors or the key — honestly labelled, since it is L2-only, not semantically secure, and leaks the approximate distance ordering by design. `v0.11.0` adds a **semantically secure** client-side mode (`vector_encryption="client_side"`): the server stores only XChaCha20-Poly1305 ciphertext plus a zero placeholder, learns nothing about the vectors (genuinely IND-CPA), and does no ranking — so the client fetches the (optionally pre-filtered) set and ranks locally, with native Rust/Python/TypeScript ciphers validated by a bit-exact cross-language test. `v0.12.0` is a documentation & packaging fix — it corrects the install guidance (build from source; the `quiver-cli` name on crates.io is an unrelated project, and publishing the SDKs is a roadmap item) and the README rendering, with no functional change. `v0.13.0` brings **incremental updates to the last index family that still rebuilt on every write**: the Vamana and disk-resident DiskVamana graphs now use **FreshDiskANN's StreamingMerge** — a read-only base graph plus a small in-memory delta graph and an `O(1)` deletion set, searched together and consolidated by a derived rebuild past a churn threshold — so graph writes become size-independent while the index stays derived and the `kill -9` crash gate is untouched. `v0.14.0` takes two of the multi-vector / ColBERT follow-ups: document upsert/delete now maintain the token-pool index **incrementally** (no full rebuild), and an opt-in `colbert` index adds **ColBERTv2 residual compression + PLAID centroid pruning** for `multivector` collections — both derived, so the crash gate stays untouched; native variable-stride document rows are deferred pending a reference-hardware locality measurement. `v0.15.0` ships the documentation polish before launch: an **mdBook documentation site** under `apps/docs` (concepts → quickstart → self-hosting → features → API/SDKs → security → architecture), a verified clean-clone quickstart, a **native TypeScript DCPE cipher** (closing the last SDK gap), and the two deferred **DCPE Scale-And-Perturb hardening** steps — a key-derived component shuffle and an ordering-preserving global normalisation — shipped as a breaking cipher v2 across Rust/Python/TypeScript with a cross-language known-answer test, honest about the one limit it cannot cross (per-axis whitening would break searchable ordering). `v0.16.0` makes the **terminal cockpit** the headline: a coherent **Bronze Quiver** retro brand (bronze chrome, leather borders, parchment text, a verdigris accent, on oak-black), a **logo whose V is a 3-D arrowhead**, and a vocabulary of minimalist retro decorations — framed panels, a database drum icon, a collections table with per-row load bars, a points-trend sparkline, a relationship tree, status badges, and a severity-tagged activity log — so an operator grasps the data, its structure, and what is happening at a glance; the view code is decoupled behind a render-to-buffer API, and a workspace-isolated generator renders each screen to a committed PNG (`just tui-shots`) for the README and docs from the *real* render, so the screenshots never go stale. Every performance/memory claim in this README is backed by a reproducible benchmark on documented reference hardware — until those numbers are recorded, that table stays empty rather than guess.

## Why Quiver

Native-Rust vector databases already exist; Quiver is not trying to out-scale Milvus or out-feature Qdrant. Its defensible edge is the **combination** of three things, executed well:

- **Security-first, by default** — encryption-at-rest is on out of the box, sealing every durable byte (segments, manifest, **and** the write-ahead log) with XChaCha20-Poly1305; payloads can be client-side-encrypted so the server never sees them; API-key scopes, RBAC, tenant isolation, audit, and crypto-shredding. Only audited cryptography (RustCrypto AEAD/KDF + `rustls`) — never a home-grown primitive. The parsers that touch untrusted input (the search-filter wire format and the on-disk page/WAL decoders) are [fuzzed](./docs/security/fuzzing.md).
- **Memory frugality** — a disk-resident graph index (DiskANN/Vamana) plus quantization (product / scalar / binary) serve large datasets from a laptop's RAM budget. The headline metric is **memory at a fixed recall**.
- **Developer experience** — a single static binary; embeddable *and* server modes; a `ratatui` cockpit with a 2-D constellation view of the vector space; idiomatic Python/TypeScript SDKs; an MCP server so agents can drive it.

We say plainly what we do **not** do: client-side payload encryption protects *payloads, not vectors* (the experimental, opt-in [DCPE mode](./docs/security/dcpe.md) addresses vectors — a published scheme that, by design, leaks the approximate distance-comparison relation and is not semantically secure); billion-scale needs a server, while a laptop comfortably serves tens-to-hundreds of millions; there is no homomorphic search in core. See the honest [threat model](./docs/security/threat-model.md).

> *The name.* A quiver holds arrows, and an arrow is a vector — apt for a database of them. And in mathematics a *quiver* is a directed graph, which is exactly what an HNSW or Vamana index is. The cockpit wears that identity in **bronze** — the colour of a quiver, with the logo's V drawn as a 3-D arrowhead.

## The cockpit

A retro terminal cockpit ships in the box (`quiver tui`): a live dashboard in the **Bronze Quiver** theme — connection health and an `ONLINE`/`OFFLINE` badge, a collections table with per-collection load bars, a points-trend sparkline, the relationship view of the selected collection, and a severity-tagged activity log.

![The Quiver cockpit dashboard](docs/assets/cockpit/dashboard.png)

Press `v`/`enter` on a collection for the **constellation view** — a 2-D projection of its vector space with the query's nearest neighbour highlighted and an interactive cursor that re-queries around any point:

![The Quiver constellation view](docs/assets/cockpit/constellation.png)

The screenshots are generated from the *real* render of seeded demo data with `just tui-shots` (a dev-only, workspace-isolated tool), so they regenerate in one command and never go stale ([ADR-0036](./docs/adr/0036-retro-cockpit-design-system.md)).

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
cargo run -p quiver-cli -- --help
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

CI workflows exist under [`.github/workflows`](.github/workflows) but are **manual-only** (`workflow_dispatch`) by design — the authoritative gate is local `just verify` ([ADR-0015](./docs/adr/0015-ci-policy.md)).

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
| **recall@10** | 0.794 | 0.898 | 0.960 | 0.987 | 0.996 |
| **QPS** (1 thread) | 1150 | 1032 | 870 | 673 | 508 |
| **p95 latency** (ms) | 1.1 | 1.2 | 1.5 | 1.9 | 2.7 |

**Head-to-head on SIFT1M**, every system on the *same* box (i7-12700H · 20 threads · 15.5 GB), peak single-thread QPS at **recall@10 ≥ 0.95** (full method, sweeps, and the wins/losses matrix: [`comparison-v0.18.0`](./docs/benchmarks/results/comparison-v0.18.0/comparison-v0.18.0.md)):

| System | recall@10 | QPS (1T) | p95 (ms) | RSS (MB) | build |
|---|---:|---:|---:|---:|---:|
| FAISS 1.14 | 0.968 | **2900** | 0.5 | 1234 ¹ | 110 s |
| **Quiver v0.18** | 0.960 | **870** | **1.5** | 1617 | ≈14 min ² |
| Chroma 1.5 | 0.977 | 743 | 2.1 | 3534 ¹ | 202 s |
| Milvus 2.5 (server) | 0.987 | 522 | 2.8 | 1254 | 31 s |
| Weaviate 1.27 | 0.983 | 506 | 2.6 | 2161 | 40 min |
| Qdrant 1.13 | 0.993 | 337 | 5.7 | **259** ³ | 118 s |
| LanceDB 0.33 | 0.557 ⁴ | 159 | 7.8 | 2255 ¹ | 19 s |

Quiver is **second only to FAISS** on both throughput and tail latency at this recall bar, with recall on par with the field — a strong result for the in-memory path.

The honesty that makes the table trustworthy: this is an **in-memory HNSW** comparison for *every* system, so RSS here is full-vectors-in-RAM — Quiver's memory-frugality wedge is its **disk-resident path** (only PQ codes resident; ~32× less RAM, see [`disk-path.md`](./docs/benchmarks/results/disk-path.md)), **not** this table. ¹ FAISS/Chroma/LanceDB run in-process so their RSS includes the Python harness + the resident 512 MB dataset (inflated; only Quiver/Milvus/Qdrant/Weaviate report the isolated DB). ² Quiver's "build" is the **REST-upload** path (1M batched POSTs); a bulk-ingest endpoint is a follow-up and it does not reflect engine speed. ³ Qdrant mmaps vectors to disk by default. ⁴ LanceDB's IVF-PQ config doesn't reach 0.95 recall in this sweep (shown at its best). Numbers are **dev-box, indicative** — comparative standings on the identical box are real (per the [methodology](./docs/benchmarks/methodology.md)); **absolute** RSS and the 10M disk path are reference-hardware-pending; we never fabricate. Milvus is benchmarked as the **server** (Docker), not the in-process Lite build.

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
- **Security policy** — [`SECURITY.md`](./SECURITY.md) · **Threat model** — [`docs/security/threat-model.md`](./docs/security/threat-model.md)
- **Contributing** — [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- **License** — [AGPL-3.0-only](./LICENSE)
