# Changelog

All notable changes to Quiver are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Quiver is pre-1.0: minor releases ship coherent, owner-gated feature sets and
may include pre-1.0 API refinements. See [`docs/roadmap.md`](docs/roadmap.md)
for the per-release rationale and Definitions of Done.

## [Unreleased]

## [0.21.0] — 2026-06-23

### Added

- Packaging & distribution pipeline (ADR-0056): backfilled `CHANGELOG.md`, crate
  publish metadata, secret-gated crates.io / PyPI / npm publish jobs, and a Helm
  chart + Kubernetes manifests under `infra/`.
- TypeScript SDK parity with the Python async client: `upsertIter` (batches a
  sync or async iterable), `scroll` (an async generator for export / re-embedding),
  and `deleteByFilter` (paged erasure).
- Go SDK bulk/maintenance helpers: `UpsertBatch` (batched upload), `Scroll` (page
  through a collection via a callback), and `DeleteByFilter` (paged erasure) — all
  context-aware, standard-library only.

### Changed

- Concurrent reads (ADR-0057): the server serves searches behind a reader–writer
  lock instead of a single mutex, so reads run in parallel. The engine gains
  `&self` `search_snapshot` / `hybrid_search_snapshot` /
  `search_multi_vector_snapshot` reads and `ensure_indexed`; the single-writer
  model, durability, and crash gate are unchanged. Fully lock-free arc-swap
  snapshots are the staged successor.

### Fixed

- Go SDK `Fetch` parsed the wrong response envelope (`matches` instead of the
  `points` the fetch endpoint returns), so it never returned points; now fixed,
  with a regression test.

## [0.20.1] — 2026-06-23

### Changed

- Re-ran the multi-DB benchmark on the v0.20.0 engine with the bulk-ingest build
  path so the build column is an honest *time-until-queryable* (ADR-0055); folded
  SIFT1M + GIST1M results across seven competitors into the README, docs, and the
  "Quiver, Explained" field guide.

### Fixed

- "Quiver, Explained" PDF layout: figure overlaps and the ColBERT callout page break.

## [0.20.0] — 2026-06-23

### Added

- Online snapshot & restore — consistent whole-directory copy over REST and the
  MCP server (ADR-0050).
- Client-streaming gRPC `UpsertStream` (ADR-0045 fast-follow).
- Real Prometheus `/metrics` (counters + latency histogram + security counters),
  request tracing spans, and an importable Grafana dashboard (ADR-0054).
- Per-key rate limiting — token bucket with `RateLimit-*` headers and `429`
  (ADR-0049).
- Server-side embedding & reranking hooks, provider-agnostic and opt-in per
  collection (ADR-0047); BM25 with a Snowball (Porter2) stemmer (ADR-0048).
- A standard-library Go SDK mirroring the REST surface; `snapshot()` parity in
  the Python and TypeScript SDKs.
- Design-only ADRs for the big bets: distributed/sharded mode (0051), GPU
  acceleration (0052), and lock-free MVCC reads (0053).

## [0.19.0] — 2026-06-22

### Added

- Hybrid (dense + sparse) search on every surface — gRPC, MCP, and the
  TypeScript SDK join REST and Python — backed by a derived sparse inverted
  index; bulk ingest via `POST …/points:bulk` (ADR-0045).

### Fixed

- De-noised CI by isolating and retrying the virtualization-sensitive
  crash-recovery test.

## [0.18.1] — 2026-06-19

### Fixed

- Automated, tag-triggered Windows release asset so `quiver update` resolves on
  Windows; unified multi-platform release packaging (ADR-0044).

## [0.18.0] — 2026-06-19

### Added

- Query cost limits enforced at the op layer (caps on `k`, `ef_search`,
  dimension, payload, batch), closing an authenticated-DoS vector (ADR-0040).
- Deep, large-data benchmark dimensions with real SIFT1M and GIST1M multi-DB
  comparisons and a Docker Milvus-server adapter (ADR-0041).
- RAG/agentic ergonomics — async Python client, batched upsert / scroll /
  delete-by-filter, a Haystack `DocumentStore`, a `rerank` helper, and an MCP
  `collection_info` tool (ADR-0042).
- Hybrid (dense + sparse) search with RRF fusion over the engine, REST, and the
  Python SDK (ADR-0043).

## [0.17.2] — 2026-06-19

### Fixed

- Windows install/update hotfix (`fsync_dir` behavior) and CDN stale-asset cache bypass.

## [0.17.1] — 2026-06-19

### Changed

- Verdigris "V" arrowhead in all terminal banners matching the TUI logo;
  regenerated cockpit screenshots.

## [0.17.0] — 2026-06-18

### Added

- One-command install (`install.sh` / `install.ps1`) and a self-updating
  `quiver update` subcommand with checksum verification (ADR-0039).
- Scientific multi-DB benchmark suite (ADR-0037).

### Changed

- 35× faster upsert build time via batched WAL sync (ADR-0038).

## [0.16.0] — 2026-06-18

### Added

- The retro cockpit (ADR-0036): a coherent Bronze Quiver brand, a logo whose "V"
  is a 3-D arrowhead, a vocabulary of retro decorations, a render-to-buffer view
  API, and reproducible committed screenshots (`just tui-shots`).

## [0.15.0] — 2026-06-17

### Added

- mdBook documentation site under `apps/docs`; a native TypeScript `DcpeCipher`
  closing the last DCPE SDK gap (ADR-0035).

### Changed

- DCPE Scale-And-Perturb hardening as a breaking cipher v2 — key-derived
  component shuffle (an exact L2 isometry) plus an optional ordering-preserving
  affine normalisation; full per-axis whitening is documented as incompatible
  with searchable encryption and deliberately not offered (ADR-0035).

## [0.14.0] — 2026-06-17

### Added

- Multi-vector / ColBERT follow-ups (ADR-0034): incremental multi-vector index
  maintenance and an opt-in ColBERTv2 residual-compression index with PLAID
  centroid pruning, creatable across REST/gRPC/MCP and the SDKs. Native
  variable-stride document rows were deferred honestly, gated on a measured
  locality win.

## [0.13.0] — 2026-06-17

### Added

- Graph-index incremental updates — Vamana and DiskVamana adopt FreshDiskANN's
  StreamingMerge model (in-memory delta + `O(1)` tombstones + churn-threshold
  consolidation), the last index family that previously rebuilt on every write
  (ADR-0033).

## [0.12.0] — 2026-06-16

### Fixed

- Install honesty (the `quiver-cli` crates.io name is an unrelated crate → install
  from source), static README badges that render everywhere, and UTF-8 mojibake in
  the status banner. No functional change.

## [0.11.0] — 2026-06-16

### Added

- Semantically secure client-side vector encryption — `vector_encryption =
  client_side` seals each vector with XChaCha20-Poly1305 and stores an opaque
  blob, the server does no ranking, and retrieval is a client-side fetch-and-rank
  (ADR-0032). The flag migrated from `encrypted_vectors` to a three-valued
  `vector_encryption` enum (byte-compatible on disk).

## [0.10.0] — 2026-06-15

### Added

- Experimental DCPE vector-encryption mode — the published Scale-And-Perturb
  distance-comparison-preserving scheme, opt-in and off by default, with honest
  limits (L2-only, not semantically secure) (ADR-0031).

## [0.9.0] — 2026-06-15

### Added

- Asynchronous leader-follower replication — a follower applies the leader's
  committed op log and serves reads while refusing writes; failover and consensus
  are explicit non-goals (ADR-0030).

## [0.8.0] — 2026-06-15

### Added

- Live Chroma and Postgres migration connectors, completing one-command live
  migration from Qdrant, Chroma, and pgvector (ADR-0029).

## [0.7.0] — 2026-06-15

### Added

- Multi-vector / late-interaction (ColBERT) retrieval — `multivector` collections
  ranked by MaxSim over the existing row-addressed store, reachable from every
  surface (ADR-0028).

## [0.6.0] — 2026-06-15

### Added

- Durable on-disk incremental IVF index recovered by snapshot + WAL-tail replay
  under the manifest's atomic swap (ADR-0025); a live Qdrant migration connector
  over HTTP (ADR-0027).

## [0.5.0] — 2026-06-15

### Added

- HNSW incremental delete (`O(1)` soft-delete + amortized rebuild, ADR-0026);
  neighbor-bounded IVF reassignment (ADR-0023); a unified secure database-open
  path shared by the server, MCP, and CLI.

## [0.4.0] — 2026-06-14

### Added

- Incremental in-place IVF index updates (SpFresh/LIRE, in-memory so the crash
  gate is untouched, ADR-0023); migration importers for Qdrant / Chroma / pgvector
  exports (ADR-0024).

## [0.3.0] — 2026-06-14

### Added

- Security depth — client-side payload encryption with a documented trust
  boundary, RBAC + scoped API keys + optional mTLS, an append-only audit log,
  per-collection-DEK envelope encryption with crypto-shredding, and the cockpit
  constellation view; `cargo-fuzz` targets for the wire and on-disk parsers.

## [0.2.0] — 2026-06-14

### Added

- Memory frugality — disk-resident graph (DiskANN/Vamana) + IVF, quantization
  (PQ / scalar / binary with hamming pre-filter + exact re-rank), per-collection
  recall/latency/memory knobs, hybrid filtered search, the TypeScript SDK, the MCP
  server, and the LangChain / LlamaIndex adapters.

## [0.1.0] — 2026-06-13

### Added

- Single-node core — encrypted collection → upsert → filtered k-NN → live TUI from
  one binary; storage engine (segments + WAL + crash recovery + checksums); HNSW;
  SIMD kernels; REST + gRPC; encryption-at-rest by default; TLS via `rustls`; the
  TUI MVP; the benchmark harness with first SIFT1M numbers; the Python SDK.

[Unreleased]: https://github.com/achref-soua/quiver/compare/v0.21.0...HEAD
[0.21.0]: https://github.com/achref-soua/quiver/compare/v0.20.1...v0.21.0
[0.20.1]: https://github.com/achref-soua/quiver/compare/v0.20.0...v0.20.1
[0.20.0]: https://github.com/achref-soua/quiver/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/achref-soua/quiver/compare/v0.18.1...v0.19.0
[0.18.1]: https://github.com/achref-soua/quiver/compare/v0.18.0...v0.18.1
[0.18.0]: https://github.com/achref-soua/quiver/compare/v0.17.2...v0.18.0
[0.17.2]: https://github.com/achref-soua/quiver/compare/v0.17.1...v0.17.2
[0.17.1]: https://github.com/achref-soua/quiver/compare/v0.17.0...v0.17.1
[0.17.0]: https://github.com/achref-soua/quiver/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/achref-soua/quiver/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/achref-soua/quiver/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/achref-soua/quiver/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/achref-soua/quiver/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/achref-soua/quiver/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/achref-soua/quiver/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/achref-soua/quiver/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/achref-soua/quiver/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/achref-soua/quiver/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/achref-soua/quiver/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/achref-soua/quiver/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/achref-soua/quiver/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/achref-soua/quiver/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/achref-soua/quiver/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/achref-soua/quiver/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/achref-soua/quiver/releases/tag/v0.1.0
