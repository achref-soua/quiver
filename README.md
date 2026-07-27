<div align="center">

<img src="docs/assets/cockpit/logo.png" alt="QUIVER — the V is a 3-D arrowhead" width="460">

# Quiver

**The security-first vector database.** Client-side-encryptable, memory-frugal approximate-nearest-neighbour search that runs on a laptop — with a retro terminal cockpit.

[![license](https://img.shields.io/badge/license-AGPL--3.0-blue)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-stable-orange)](./rust-toolchain.toml)
[![CI](https://img.shields.io/badge/CI-every%20PR-brightgreen)](.github/workflows)
[![coverage](https://img.shields.io/badge/coverage-90.9%25-brightgreen)](.github/workflows/ci.yml)
[![release](https://img.shields.io/badge/release-v1.0.0-FFB000)](https://github.com/achref-soua/quiver/releases)
[![status](https://img.shields.io/badge/status-v1.0.0%20·%20stable-brightgreen)](./docs/roadmap.md)
[![stars](https://img.shields.io/badge/Star_on-GitHub-FFB000?logo=github)](https://github.com/achref-soua/quiver/stargazers)

</div>

> **Status: `v1.0.0` — stable.** The compatibility promise starts here: the **REST and gRPC wire protocols**, the **Python and TypeScript SDK APIs**, and the **on-disk format with its version gates** are stable, and breaking any of them requires a major version. Not covered, and labelled as such wherever it appears: internal crate APIs, the still-maturing cluster HTTP surface, and anything documented as experimental — the DCPE vector-encryption mode in particular.
>
> `1.0.0` is a statement about stability, not a claim of being finished. The engine is complete for the job it advertises — encrypted storage with a `kill -9` crash gate, HNSW / IVF / DiskANN with product, scalar and binary quantization, hybrid and BM25 search, MVCC reads, per-shard Raft write HA, elastic scaling in both directions, an MCP server, REST and gRPC, both SDKs, and the cockpit. What is still open is written down in the [roadmap](./docs/roadmap.md) rather than left to be discovered: the GPU kernel is wired into no build or search path yet ([ADR-0079](./docs/adr/0079-gpu-build-search-wiring.md) settles where it plugs in), and the 10M disk-path head-to-head needs a machine with more RAM than the reference laptop. Everything published below was measured on hardware described down to its load average; nothing here is estimated. Per-release history lives in the [changelog](./CHANGELOG.md).

## Why Quiver

Native-Rust vector databases already exist; Quiver is not trying to out-scale Milvus or out-feature Qdrant. Its defensible edge is the **combination** of three things, executed well:

- **Security-first, by default** — encryption-at-rest is on out of the box, sealing every durable byte (segments, manifest, **and** the write-ahead log) with XChaCha20-Poly1305; payloads can be client-side-encrypted so the server never sees them; API-key scopes, RBAC, tenant isolation, audit, and crypto-shredding. Only audited cryptography (RustCrypto AEAD/KDF + `rustls`) — never a home-grown primitive. The parsers that touch untrusted input (the search-filter wire format and the on-disk page/WAL decoders) are [fuzzed](./docs/security/fuzzing.md), and the whole codebase is reviewed in a documented [security audit](./docs/security/audit-0.29.0.md) — a static OWASP-style review **plus a dynamic [OWASP ZAP](https://www.zaproxy.org/) scan** of a live server and the fuzzers re-run — with every finding fixed and regression-tested and the residual risks stated honestly.
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

> **→ [Read the PDF field guide](./docs/quiver-explained.pdf)** &nbsp;·&nbsp; 60 pages, fully illustrated &nbsp;·&nbsp; also available as [Markdown](./docs/quiver-explained.md)

## Architecture

A Cargo workspace: a from-scratch storage engine, index structures, SIMD distance kernels, and query planner, with a thin gRPC/REST shell and a TUI client. One binary runs the server, the cockpit, and the MCP server.

→ [System context](./docs/architecture/c4-context.md) · [Container view](./docs/architecture/c4-container.md) · [Overview & crate map](./docs/architecture/overview.md) · [ADRs](./docs/adr) · [State of Quiver (assessment)](./docs/analysis/state-of-quiver-v0.17.md)

## Quickstart

> **Full documentation** lives at **[achref-soua.github.io/quiver](https://achref-soua.github.io/quiver/)** (an mdBook — source under [`apps/docs`](./apps/docs), build it locally with `just docs`) — concepts, self-hosting, every feature, the API/MCP/SDK references, the security docs, and an architecture deep dive.

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
icon embedded natively. To pin a specific version, set `QUIVER_VERSION=1.0.0` before running.

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

Seeds two collections — a text-searchable `articles` set and 1 000 synthetic vectors
for the constellation — starts the REST server on `:7333`, and opens the retro
cockpit, where every op (browse, constellation, text search) works offline against the
seeded data. No config files, no env vars, no external downloads.

**Full server quick start:**

```bash
quiver serve            # gRPC + REST on :6333, encrypted by default
quiver tui              # the retro cockpit
quiver mcp              # MCP server (stdio) so AI agents can drive Quiver
```

**Or install from a package registry:**

```bash
cargo install quiverdb-cli          # the `quiver` binary, from crates.io
pip install quiver-client           # the Python SDK, from PyPI
npm install quiver-client           # the TypeScript SDK, from npm
```

The crates publish under the `quiverdb-*` namespace ([ADR-0056](./docs/adr/0056-packaging-and-distribution.md)) — `cargo install quiverdb-cli` installs the same `quiver` binary the script above downloads. (The pre-built binary is the fastest path; `cargo install` compiles from source.)

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
around any point. A recorded tour of exactly that — the dashboard, the keybindings,
a 256-point constellation and a re-query — ships at
[`docs/assets/cockpit.cast`](./docs/assets/cockpit.cast); play it with
`asciinema play docs/assets/cockpit.cast`, or re-record it in one command with
`scripts/record-cockpit-cast.sh` (interactive on a TTY, scripted anywhere else).
To build and exercise the workspace directly:

```bash
just build            # compile the workspace
just verify           # the full local quality gate (lint · test · doc · deny · audit)
cargo run -p quiverdb-cli -- --help
```

> **Heads-up:** Quiver's CLI publishes as **`quiverdb-cli`** (`cargo install quiverdb-cli`) — the
> `quiver-cli` name on crates.io is an unrelated third-party project, which is why the `quiverdb-*`
> namespace is used ([ADR-0056](./docs/adr/0056-packaging-and-distribution.md)).

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
| `just acceptance` | boot a real server and drive every surface (REST · both SDKs · CLI · MCP) |
| `just bench *ARGS` | run the benchmark harness (e.g. `just bench --synthetic`) |
| `just coverage` | HTML coverage report (CI gates the same measurement at 85%) |
| `just fuzz <target> <secs>` | fuzz a parser (`filter_json` · `page_decode` · `wal_decode`) |
| `just docker` | build the container image |

The `ci` and `security` workflows under [`.github/workflows`](.github/workflows) run on every pull request and on pushes to `main`/`develop`; the heavier `build` workflow stays manual (`workflow_dispatch`). `ci` gates **fmt · clippy · test · doc**, a **coverage** floor of 85% lines (`cargo llvm-cov`), the **acceptance** run below, `helm lint`, and crate publishability; `security` gates **deny · audit · gitleaks**. Local `just verify` runs the same fast pre-commit steps, so the two never drift ([ADR-0015](./docs/adr/0015-ci-policy.md)).

The **acceptance** job is the one worth knowing about: it boots a real encryption-at-rest server and drives every external surface the way an operator does — REST, both SDKs, the CLI importer, and the MCP server over stdio. Both SDK unit suites mock their transport, so they prove the client's shape but never that it matches what the server sends; the acceptance run is what crosses a real socket.

## SDK & benchmarks

The **Python SDK** is on PyPI as [`quiver-client`](https://pypi.org/project/quiver-client/) (`pip install quiver-client`; or `pip install ./sdks/python` from a checkout):

```python
from quiver import Client, Point

with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("items", dim=3, metric="cosine")
    q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
    hits = q.search("items", [0.1, 0.2, 0.3], k=5)
```

The **TypeScript SDK** is on npm as [`quiver-client`](https://www.npmjs.com/package/quiver-client) (`npm install quiver-client`; or `pnpm add ./sdks/typescript` from a checkout), dependency-free over the global `fetch`, and can pick the memory-frugal disk index:

```ts
import { Client } from "quiver-client";

const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });
await q.createCollection("items", 3, { metric: "cosine", index: "disk_vamana", pqSubspaces: 1 });
await q.upsert("items", [{ id: "a", vector: [0.1, 0.2, 0.3], payload: { tag: "x" } }]);
const hits = await q.search("items", [0.1, 0.2, 0.3], { k: 5 });
```

A **LangChain** `VectorStore` adapter ships in `quiver.langchain` (`pip install "quiver-client[langchain]"`), and a **LlamaIndex** `VectorStore` in `quiver.llamaindex` (`pip install "quiver-client[llamaindex]"`) — so any Quiver index, including the memory-frugal disk path, backs a LangChain or LlamaIndex retriever. The LlamaIndex adapter maps `MetadataFilters` onto Quiver's hybrid pre-filter. A synchronous `Client` and an async `AsyncClient` share one contract, with batched-upsert/scan/delete-by-filter helpers for ingestion and erasure.

**Using Quiver in RAG / agents.** End-to-end guides — [RAG](./apps/docs/src/guides/rag.md) (chunk → embed → filtered search → rerank → answer), [agentic patterns over MCP](./apps/docs/src/guides/agentic.md), and [tuning for RAG](./apps/docs/src/guides/tuning.md) (index/quantizer/recall-RAM) — plus a runnable [`examples/rag/quickstart.py`](./examples/rag/quickstart.py) that needs no API key.

**Client-side payload encryption** (ADR-0012): seal payload fields with a key the server never sees, so it stores and returns only ciphertext, while cleartext sibling fields stay server-filterable. The `PayloadCipher` helper ships in both SDKs (`quiver.encryption` / `quiver-client/encryption`) and a Rust reference (`quiver_crypto::payload`), sharing one XChaCha20-Poly1305 envelope byte-for-byte. The trust boundary is honest — it protects payloads, not vectors — and proven by a test that runs a server with at-rest encryption off and shows the sealed field never appears in plaintext over the API or on disk.

An `ann-benchmarks`-style harness lives in [`bench/`](./bench). On **SIFT1M** (1M × 128, L2), in-memory HNSW (`M=16`, `efC=200`), Quiver's own recall ↔ throughput ↔ latency curve:

| `ef_search` | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| **recall@1** | 0.853 | 0.928 | 0.966 | 0.984 | 0.988 |
| **recall@10** | 0.793 | 0.895 | 0.958 | 0.986 | 0.995 |
| **recall@100** | 0.918 | 0.918 | 0.918 | 0.944 | 0.983 |
| **QPS** (1 thread) | 675 | 726 | 625 | 553 | 419 |
| **QPS** (8 threads) | 649 | 592 | 644 | 626 | 600 |
| **p95 latency** (ms) | 2.4 | 1.9 | 2.3 | 2.5 | 3.3 |

`ef_search` is the one knob: it buys recall with latency. Read the concurrency row
honestly — a single-process Python client is itself a ceiling, so *light* queries are
client-bound and 8 threads buy nothing; the server-side win only shows on heavier
queries, reaching **1.43× at `ef=256`**, not a flattering "8×".

**Head-to-head on SIFT1M** (`v1.0.0` run, [full results + sweeps](./docs/benchmarks/results/comparison-v1.0.0/comparison-v1.0.0.md)). Every system measured in the same run, on the same data, on the documented reference hardware below — peak single-thread QPS at **recall@10 ≥ 0.95**:

| System | recall@1 | recall@10 | recall@100 | QPS (1T) | QPS (8T) | p95 (ms) | RSS (MB) | build (s) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| FAISS 1.14 | 0.976 | 0.968 | 0.868 | **2998** | **8198** | **0.45** | 2181 ¹ | 142 |
| **Quiver v1.0** | 0.966 | 0.958 | 0.918 | **625** | 644 | **2.28** | 1454 | 807 ² |
| Qdrant 1.13 | 0.979 | 0.977 | 0.937 | 267 | 329 | 6.58 | **263** ³ | 182 |
| LanceDB 0.33 | 0.459 | 0.557 ⁴ | 0.634 | 157 | 308 | 7.99 | 1675 ¹ | 42 |

**Reference hardware:** 12th Gen Intel Core i7-12700H · 10 physical / 20 logical cores · 15.5 GB RAM (10.8 GB available) · 4 GB swap · WSL2 (kernel 6.6.87.2) · Ubuntu 22.04.5. That is **a laptop**, and it is labelled as one: these are reproducible comparative results, not datacentre numbers. Every run records its own machine manifest — CPU model, core counts, free RAM, load average at start, and the exact Quiver binary — so a run taken on a busy machine can be recognised rather than quietly published.

Quiver is **second only to FAISS** on both throughput and tail latency at this recall bar. FAISS is an in-process library that skips the network entirely; Quiver is answering over HTTP. Qdrant reaches slightly higher recall at ~2.3× lower throughput, and holds far less RAM because it mmaps vectors to disk by default — which is the honest comparison to make, since **this table is an in-memory HNSW comparison for every system**. Quiver's memory-frugality wedge is its **disk-resident path** (only quantized codes resident, ~32× less RAM — see [`disk-path.md`](./docs/benchmarks/results/disk-path.md)), *not* this table.

¹ FAISS and LanceDB run in-process, so their RSS includes the Python harness and the resident 512 MB dataset — inflated; only Quiver and Qdrant report an isolated server. ² Quiver's "build" is *time-until-queryable* over the bulk-ingest path ([ADR-0045](./docs/adr/0045-hybrid-everywhere-and-fast-ingest.md)) — one WAL fsync per request plus a single deferred index pass, with the first query forcing and now **awaiting** that build ([ADR-0081](./docs/adr/0081-index-readiness.md)); before ADR-0081 the forcing query returned without waiting, so part of the build fell outside the timer. ³ Qdrant mmaps vectors to disk by default. ⁴ LanceDB's IVF-PQ config does not reach 0.95 recall in this sweep and is shown at its best point — an honest DNF, not a tuned-down row.

Two things worth stating rather than burying. **Recall is unchanged across sixteen releases**: the `ef` sweep reproduces the v0.22.0 curve to three decimal places (0.793 / 0.895 / 0.958 / 0.986 / 0.995), which is a stronger correctness signal than any single number in the table. And **single-thread QPS is below the v0.22.0 run** while RSS fell from ~2069 MB to 1454 MB. Those two moves are consistent with the on-disk resident-state work of [ADR-0072](./docs/adr/0072-on-disk-primary-index.md)/[ADR-0073](./docs/adr/0073-on-disk-embed-id-map.md), which deliberately trades RAM for lookups — but that attribution is **not isolated by a controlled experiment**, so it is offered as the likely explanation, not as a measured fact.

**GIST1M** (1M × 960, L2) is the harder, higher-dimensional test. These figures are from the **`v0.20.0`** run and were **not re-measured for `v1.0.0`** — the SIFT1M table above is the current one. Same box, each system at its most efficient config reaching recall@10 ≥ 0.95, or its best point at `ef_search ≤ 256` (960-d needs a wide beam, so most plateau below 0.95 in this sweep):

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

The **disk-resident path** is the memory-frugality wedge. On SIFTSMALL (128-d), it serves recall@10 up to **1.000** while holding only PQ codes in RAM — a **32× smaller RAM-resident footprint** than full-precision vectors (the graph and vectors live in the encrypted on-disk index). That reduction is exact arithmetic and scales (e.g. a 10M × 768-d collection: ~1 GB resident vs ~31 GB). As of `v0.23.0` the disk index is also **durable** ([ADR-0063](./docs/adr/0063-durable-disk-vamana-index.md)): a restart now *loads* the mmap'd base instead of rebuilding it from every full-precision vector, so a running server actually serves from the frugal path after a restart — earlier releases paid an `O(N)` full-RAM rebuild on open, which is what made the benchmark's post-build RSS unrepresentative. The head-to-head **RSS vs Qdrant/LanceDB** at 10M scale stays reference-hardware-pending; a Windows one-command frugality runner (`scripts/bench-disk-frugality.ps1`) measures the wedge on hardware you control. Numbers and method: [`docs/benchmarks/results/disk-path.md`](./docs/benchmarks/results/disk-path.md).

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

- **Documentation site** — **[achref-soua.github.io/quiver](https://achref-soua.github.io/quiver/)** (mdBook; source [`apps/docs`](./apps/docs), `just docs`)
- **Roadmap & Definitions of Done** — [`docs/roadmap.md`](./docs/roadmap.md)
- **Changelog** — [`CHANGELOG.md`](./CHANGELOG.md)
- **Security policy** — [`SECURITY.md`](./SECURITY.md) · **Threat model** — [`docs/security/threat-model.md`](./docs/security/threat-model.md) · **Security audit (OWASP ZAP + static + fuzz)** — [`docs/security/audit-0.29.0.md`](./docs/security/audit-0.29.0.md)
- **Contributing** — [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- **License** — [AGPL-3.0-only](./LICENSE)
