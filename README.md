<div align="center">

# Quiver

**The security-first vector database.** Client-side-encryptable, memory-frugal approximate-nearest-neighbour search that runs on a laptop — with a retro terminal cockpit.

[![license](https://img.shields.io/github/license/achref-soua/quiver?color=blue)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-stable-orange)](./rust-toolchain.toml)
[![CI](https://img.shields.io/badge/CI-manual%20dispatch-informational)](.github/workflows)
[![release](https://img.shields.io/github/v/release/achref-soua/quiver?color=FFB000&label=release)](https://github.com/achref-soua/quiver/releases)
[![status](https://img.shields.io/badge/status-v0.3.0%20·%20phase%204-FFB000)](./docs/roadmap.md)
[![stars](https://img.shields.io/github/stars/achref-soua/quiver?style=flat)](https://github.com/achref-soua/quiver/stargazers)

</div>

> **Status: `v0.3.0` released · Phase 4 (advanced features) in progress.** Phase 1 (`v0.1.0`) shipped the single-node core — an encrypted, crash-safe storage engine, HNSW, SIMD kernels, REST/gRPC, the TUI, and the Python SDK. Phase 2 (`v0.2.0`) delivered memory frugality: the disk-resident DiskANN/Vamana and IVF indexes with product/scalar/binary quantization, a row-addressed storage engine (stride-addressed vector columns, paged payload heaps, roaring tombstones, compaction, secondary indexes), **hybrid filtered search**, the TypeScript SDK, the MCP server, and LangChain/LlamaIndex adapters. Phase 3 (`v0.3.0`) added security depth and cockpit polish: client-side payload encryption, RBAC with scoped API keys and optional mTLS, an append-only audit log, per-collection-DEK encryption with crypto-shredding, master-key-file secret handling, the 2-D **constellation view**, and `cargo-fuzz` targets. Phase 4 (toward `v0.4.0`) is shipping the advanced backlog — **incremental in-place index updates** (SpFresh/LIRE for IVF) have landed on `develop`. Every performance/memory claim in this README is backed by a reproducible benchmark on documented reference hardware — until those numbers are recorded, that table stays empty rather than guess.

## Why Quiver

Native-Rust vector databases already exist; Quiver is not trying to out-scale Milvus or out-feature Qdrant. Its defensible edge is the **combination** of three things, executed well:

- **Security-first, by default** — encryption-at-rest is on out of the box, sealing every durable byte (segments, manifest, **and** the write-ahead log) with XChaCha20-Poly1305; payloads can be client-side-encrypted so the server never sees them; API-key scopes, RBAC, tenant isolation, audit, and crypto-shredding. Only audited cryptography (RustCrypto AEAD/KDF + `rustls`) — never a home-grown primitive. The parsers that touch untrusted input (the search-filter wire format and the on-disk page/WAL decoders) are [fuzzed](./docs/security/fuzzing.md).
- **Memory frugality** — a disk-resident graph index (DiskANN/Vamana) plus quantization (product / scalar / binary) serve large datasets from a laptop's RAM budget. The headline metric is **memory at a fixed recall**.
- **Developer experience** — a single static binary; embeddable *and* server modes; a `ratatui` cockpit with a 2-D constellation view of the vector space; idiomatic Python/TypeScript SDKs; an MCP server so agents can drive it.

We say plainly what we do **not** do: client-side encryption protects *payloads, not vectors*; billion-scale needs a server, while a laptop comfortably serves tens-to-hundreds of millions; there is no homomorphic search in core. See the honest [threat model](./docs/security/threat-model.md).

> *The name.* A quiver holds arrows, and an arrow is a vector — apt for a database of them. And in mathematics a *quiver* is a directed graph, which is exactly what an HNSW or Vamana index is. The cockpit wears that identity in amber phosphor.

## Architecture

A Cargo workspace: a from-scratch storage engine, index structures, SIMD distance kernels, and query planner, with a thin gRPC/REST shell and a TUI client. One binary runs the server, the cockpit, and the MCP server.

→ [System context](./docs/architecture/c4-context.md) · [Container view](./docs/architecture/c4-container.md) · [Overview & crate map](./docs/architecture/overview.md) · [ADRs](./docs/adr)

## Quickstart

Pre-built binaries and container images are on the roadmap; today, build from source:

```bash
# prerequisites: rustup (stable), just (cargo install just), and uv
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

```bash
# planned (build from source today):
cargo install quiver-cli       # or: docker run ghcr.io/achref-soua/quiver
quiver serve                   # gRPC + REST, encrypted by default
quiver tui                     # the cockpit
quiver mcp                     # MCP server (stdio) so AI agents can drive Quiver
```

The [MCP server](./docs/mcp.md) exposes `create_collection`, `upsert`, `search`,
`get`, and `delete` as tools over JSON-RPC stdio, operating an encrypted
in-process database.

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

The **Python SDK** lives in [`sdks/python`](./sdks/python) (`uv add quiver-client`):

```python
from quiver import Client, Point

with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("items", dim=3, metric="cosine")
    q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
    hits = q.search("items", [0.1, 0.2, 0.3], k=5)
```

The **TypeScript SDK** lives in [`sdks/typescript`](./sdks/typescript) (`pnpm add quiver-client`), dependency-free over the global `fetch`, and can pick the memory-frugal disk index:

```ts
import { Client } from "quiver-client";

const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });
await q.createCollection("items", 3, { metric: "cosine", index: "disk_vamana", pqSubspaces: 1 });
await q.upsert("items", [{ id: "a", vector: [0.1, 0.2, 0.3], payload: { tag: "x" } }]);
const hits = await q.search("items", [0.1, 0.2, 0.3], { k: 5 });
```

A **LangChain** `VectorStore` adapter ships in `quiver.langchain` (`pip install quiver-client[langchain]`), and a **LlamaIndex** `VectorStore` in `quiver.llamaindex` (`pip install quiver-client[llamaindex]`) — so any Quiver index, including the memory-frugal disk path, backs a LangChain or LlamaIndex retriever. The LlamaIndex adapter maps `MetadataFilters` onto Quiver's hybrid pre-filter.

**Client-side payload encryption** (ADR-0012): seal payload fields with a key the server never sees, so it stores and returns only ciphertext, while cleartext sibling fields stay server-filterable. The `PayloadCipher` helper ships in both SDKs (`quiver.encryption` / `quiver-client/encryption`) and a Rust reference (`quiver_crypto::payload`), sharing one XChaCha20-Poly1305 envelope byte-for-byte. The trust boundary is honest — it protects payloads, not vectors — and proven by a test that runs a server with at-rest encryption off and shows the sealed field never appears in plaintext over the API or on disk.

An `ann-benchmarks`-style harness lives in [`bench/`](./bench). On **SIFT1M** (1M × 128, L2), in-memory HNSW (`M=16`, `efC=200`), recall@10 vs exact ground truth:

| `ef_search` | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| **recall@10** | 0.794 | 0.898 | 0.960 | 0.987 | 0.996 |

Recall is a property of the index and the data (host-independent), so these figures stand; reproduce with `cargo run --release --example sift_recall` ([details](./docs/benchmarks/results/sift1m.md)). **Throughput, memory (RSS — the headline metric), and the head-to-head vs Qdrant/LanceDB are reference-hardware-pending**: per the [methodology](./docs/benchmarks/methodology.md) those require identical dedicated hardware, this shared dev box is not a source for them, and we never fabricate.

The per-collection **recall ↔ latency ↔ memory** knobs — quantizers (scalar/product/binary), the disk-resident DiskANN path, and IVF — are documented with a tradeoff table in [`docs/benchmarks/quantization-tradeoffs.md`](./docs/benchmarks/quantization-tradeoffs.md).

The **IVF** index also supports **incremental in-place updates**: inserts, in-place updates, and deletes are applied to the live index with SpFresh-style LIRE rebalancing (cell split/merge), so streaming workloads avoid an `O(N)` rebuild ([ADR-0023](./docs/adr/0023-incremental-in-place-updates.md)).

The **disk-resident path** is the memory-frugality wedge. On SIFTSMALL (128-d), it serves recall@10 up to **1.000** while holding only PQ codes in RAM — a **32× smaller RAM-resident footprint** than full-precision vectors (the graph and vectors live in the encrypted on-disk index). That reduction is exact arithmetic and scales (e.g. a 10M × 768-d collection: ~1 GB resident vs ~31 GB). The head-to-head **RSS vs Qdrant/LanceDB** is reference-hardware-pending. Numbers and method: [`docs/benchmarks/results/disk-path.md`](./docs/benchmarks/results/disk-path.md).

## Configuration

Every option is an environment variable with a secure default; see [`.env.example`](./.env.example) and [ADR-0013](./docs/adr/0013-config-and-secure-defaults.md). Encryption-at-rest is on by default: the server requires a 256-bit key in `QUIVER_ENCRYPTION_KEY` (generate one with `openssl rand -hex 32`) unless `QUIVER_INSECURE=true`, and seals segments, the manifest, and the WAL alike. That key is a **master key** that wraps a per-collection data-encryption key (envelope encryption, [ADR-0010](./docs/adr/0010-crypto-envelope-aead.md)), so dropping a collection **crypto-shreds** it — its key is destroyed and its data becomes unrecoverable, even from a backup ([details](./docs/security/crypto.md)). TLS is required for any non-loopback bind.

**Access control (ADR-0011):** authentication is by API key and authorization is **default-deny RBAC**. A bare `QUIVER_API_KEYS` secret is an all-collections admin key; for least privilege, define scoped keys in `quiver.toml` with a `role` (`read` ⊆ `write` ⊆ `admin`) and a `collections` scope (exact names or a trailing-`*` prefix, e.g. `acme.*`, for per-namespace isolation). A key may only perform its role's actions within its scope — over-scope and cross-namespace access return `403`, and listing hides collections outside the scope. For an extra factor, set `QUIVER_TLS_CLIENT_CA` to require **mutual TLS**: both transports then demand a client certificate chaining to that CA. Set `QUIVER_AUDIT_LOG` to record every mutating/administrative operation and every denial to an append-only [audit log](./docs/security/audit.md) — the acting key, the action, the resource, and the outcome, **never the secret**.

## Project

- **Roadmap & Definitions of Done** — [`docs/roadmap.md`](./docs/roadmap.md)
- **Security policy** — [`SECURITY.md`](./SECURITY.md) · **Threat model** — [`docs/security/threat-model.md`](./docs/security/threat-model.md)
- **Contributing** — [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- **License** — [AGPL-3.0-only](./LICENSE)
