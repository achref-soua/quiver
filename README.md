<div align="center">

# Quiver

**The security-first vector database.** Client-side-encryptable, memory-frugal approximate-nearest-neighbour search that runs on a laptop — with a retro terminal cockpit.

[![license](https://img.shields.io/github/license/achref-soua/quiver?color=blue)](./LICENSE)
[![rust](https://img.shields.io/badge/rust-stable-orange)](./rust-toolchain.toml)
[![CI](https://img.shields.io/badge/CI-manual%20dispatch-informational)](.github/workflows)
[![status](https://img.shields.io/badge/status-alpha%20·%20phase%201-red)](./docs/roadmap.md)
[![stars](https://img.shields.io/github/stars/achref-soua/quiver?style=flat)](https://github.com/achref-soua/quiver/stargazers)

</div>

> **Status: alpha — Phase 1 in progress.** The design is complete and reviewed (see [`docs/`](./docs)); the engine is being implemented. The TUI cast and benchmark table land with `v0.1.0`. Every performance/memory claim in this README will be backed by a reproducible benchmark — until then, this section stays empty rather than guess.

## Why Quiver

Native-Rust vector databases already exist; Quiver is not trying to out-scale Milvus or out-feature Qdrant. Its defensible edge is the **combination** of three things, executed well:

- **Security-first, by default** — encryption-at-rest is on out of the box, sealing every durable byte (segments, manifest, **and** the write-ahead log) with XChaCha20-Poly1305; payloads can be client-side-encrypted so the server never sees them; API-key scopes, RBAC, tenant isolation, audit, and crypto-shredding. Only audited cryptography (RustCrypto AEAD/KDF + `rustls`) — never a home-grown primitive.
- **Memory frugality** — a disk-resident graph index (DiskANN/Vamana) plus quantization (product / scalar / binary) serve large datasets from a laptop's RAM budget. The headline metric is **memory at a fixed recall**.
- **Developer experience** — a single static binary; embeddable *and* server modes; a `ratatui` cockpit; idiomatic Python/TypeScript SDKs; an MCP server so agents can drive it.

We say plainly what we do **not** do: client-side encryption protects *payloads, not vectors*; billion-scale needs a server, while a laptop comfortably serves tens-to-hundreds of millions; there is no homomorphic search in core. See the honest [threat model](./docs/security/threat-model.md).

## Architecture

A Cargo workspace: a from-scratch storage engine, index structures, SIMD distance kernels, and query planner, with a thin gRPC/REST shell and a TUI client. One binary runs the server, the cockpit, and the MCP server.

→ [System context](./docs/architecture/c4-context.md) · [Container view](./docs/architecture/c4-container.md) · [Overview & crate map](./docs/architecture/overview.md) · [ADRs](./docs/adr)

## Quickstart

Published binaries and images arrive with `v0.1.0`. Today, build from source:

```bash
# prerequisites: rustup (stable), just (cargo install just), and uv
git clone https://github.com/achref-soua/quiver
cd quiver
just demo             # build, start an encrypted server, seed a demo collection
# then, in another terminal:
quiver tui --api-key quiver-demo-key   # the retro cockpit
```

`just demo` brings up a server with **encryption-at-rest on**, seeds a small
collection through the Python SDK, and prints how to open the cockpit. To build
and exercise the workspace directly:

```bash
just build            # compile the workspace
just verify           # the full local quality gate (lint · test · doc · deny · audit)
cargo run -p quiver-cli -- --help
```

```bash
# coming with v0.1.0:
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

An `ann-benchmarks`-style harness lives in [`bench/`](./bench). On **SIFT1M** (1M × 128, L2), in-memory HNSW (`M=16`, `efC=200`), recall@10 vs exact ground truth:

| `ef_search` | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|
| **recall@10** | 0.794 | 0.898 | 0.960 | 0.987 | 0.996 |

Recall is a property of the index and the data (host-independent), so these figures stand; reproduce with `cargo run --release --example sift_recall` ([details](./docs/benchmarks/results/sift1m.md)). **Throughput, memory (RSS — the headline metric), and the head-to-head vs Qdrant/LanceDB are reference-hardware-pending**: per the [methodology](./docs/benchmarks/methodology.md) those require identical dedicated hardware, this shared dev box is not a source for them, and we never fabricate.

The per-collection **recall ↔ latency ↔ memory** knobs — quantizers (scalar/product/binary), the disk-resident DiskANN path, and IVF — are documented with a tradeoff table in [`docs/benchmarks/quantization-tradeoffs.md`](./docs/benchmarks/quantization-tradeoffs.md).

## Configuration

Every option is an environment variable with a secure default; see [`.env.example`](./.env.example) and [ADR-0013](./docs/adr/0013-config-and-secure-defaults.md). Encryption-at-rest is on by default: the server requires a 256-bit key in `QUIVER_ENCRYPTION_KEY` (generate one with `openssl rand -hex 32`) unless `QUIVER_INSECURE=true`, and seals segments, the manifest, and the WAL alike. TLS is required for any non-loopback bind.

## Project

- **Roadmap & Definitions of Done** — [`docs/roadmap.md`](./docs/roadmap.md)
- **Security policy** — [`SECURITY.md`](./SECURITY.md) · **Threat model** — [`docs/security/threat-model.md`](./docs/security/threat-model.md)
- **Contributing** — [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- **License** — [AGPL-3.0-only](./LICENSE)
