# Quiver — Roadmap & Definitions of Done

Quiver is built phase by phase. A phase is not "done" until its Definition of Done (DoD) is met, tested, documented, and merged. Each phase boundary opens a release PR `develop` → `main` and tags a SemVer release.

## Gates

- **Plan gate** — the owner approves the build plan before any work. *(passed)*
- **Design gate (end of Phase 0)** — the owner approves this design package (PR-A…D) **before any implementation code is written.**
- Thereafter, each phase ships behind its DoD and a tagged release.

## Phase 0 — Design (no code) → docs only

**Goal:** a complete, reviewed technical design so implementation is de-risked on paper.

**Deliverables:** architecture + C4; on-disk format spec; index design (HNSW/Vamana/IVF + quantization) with cited papers; concurrency model; security threat model + crypto choices; API & wire-protocol spec; benchmark methodology; risk register; this roadmap; the repo-scaffold plan; ADRs for every major decision.

**DoD:** the owner has reviewed and approved the design package; the riskiest unknown — disk-resident index memory/recall behavior — has an evidence-based plan (analytical budget + a spike plan). No SemVer tag (design only).

## Phase 1 — Single-node core (usable) → `v0.1.0`

**Goal:** one binary, end-to-end: encrypted collection → upsert → filtered k-NN → live TUI.

**Scope:** Cargo workspace scaffold + `justfile` + manual CI + Dockerfile + README/CONTRIBUTING/SECURITY; storage engine (segments + WAL + crash recovery + checksums); HNSW; SIMD kernels (f32 + int8 with feature detection); REST + gRPC; basic payload filtering; encryption-at-rest secure-by-default; TLS via `rustls`; TUI MVP (live metrics + collection browser); benchmark harness + first SIFT1M numbers; Python SDK (uv); seeded demo.

**DoD (testable):**
- `quiver serve` + `quiver tui` run from one binary; `just demo` seeds an encrypted collection.
- Over **both** REST and gRPC and the Python SDK: create collection, upsert vectors with payloads, run top-k with a metadata filter, get correct results.
- Encryption-at-rest is on by default; data files are ciphertext on disk (verified by a test).
- A process **kill mid-write** recovers with no corruption and no lost acknowledged writes (crash-recovery test passes).
- Recall@10 on SIFT1M ≥ a documented threshold at a documented `ef`; numbers recorded in `docs/benchmarks/`.
- Coverage gate ≥ 70%; `just verify` green.

## Phase 2 — Memory frugality → `v0.2.0`

**Goal:** large datasets served from a small RAM budget, proven against competitors.

**Scope:** disk-resident graph (DiskANN/Vamana) + IVF; quantization (PQ/scalar/binary + hamming pre-filter + exact re-rank); recall/latency/memory knobs per collection; hybrid search (vector + filter + optional BM25); 10M+ disk-path benchmarks vs Qdrant + LanceDB; TypeScript SDK; MCP server; LangChain/LlamaIndex adapters.

**DoD:** on a 10M+ dataset, Quiver serves a documented recall@10 using a **fraction of the RAM** of Qdrant/LanceDB on identical hardware, with a reproducible methodology and raw numbers published. Quantization knobs documented with a tradeoff table. Coverage ≥ 75%.

**Status (on `develop`):** the disk-resident graph + IVF, quantization, per-collection index/quant knobs, the storage-engine rewrite (row-addressed segments, roaring tombstones, compaction, secondary indexes), **hybrid filtered search** (a selectivity planner that pre-filters to an exact scan or post-filters ANN, reachable over REST/gRPC, the MCP server, and the Python/TypeScript SDKs), and the LangChain + LlamaIndex adapters are all in and tested. The quantization/index tradeoff table and the disk-path recall/compression figures are recorded; the remaining DoD item is the published **10M reference-hardware** head-to-head (recall@10 vs RAM, against Qdrant/LanceDB) — its methodology and runbook are written, but the raw RSS/QPS require dedicated hardware and are never produced on the shared dev box or fabricated. `v0.2.0` is gated on those numbers plus owner approval.

## Phase 3 — Security depth + cockpit polish → `v0.3.0`

**Scope:** client-side payload encryption with a documented trust boundary; RBAC + scoped API keys + optional mTLS; audit logging; crypto-shredding; secret/KMS handling; the TUI **constellation view** (2D projection + nearest-neighbor highlight + interactive query); security testing (fuzz the protocol + on-disk format, `cargo audit`/`deny`, threat-model verification).

**DoD:** a payload upserted with client-side encryption is provably unreadable by the server (test); RBAC denies cross-tenant/over-scope access (tests); fuzz targets run clean for a documented duration; the cockpit demo (asciinema cast) is recorded. Coverage ≥ 80%.

**Status (shipped on `develop` for the `v0.3.0` tag):** all five slices are in and tested — client-side payload encryption (cross-language KATs); RBAC + scoped keys + optional mTLS; an append-only audit log; per-collection-DEK envelope encryption with **crypto-shredding**; `QUIVER_MASTER_KEY_FILE` secret handling; the cockpit **constellation view** (random-projection scatter + nearest-neighbour highlight + interactive re-query); and `cargo-fuzz` targets for the wire and on-disk parsers (clean over a bounded run). Core-engine coverage is **93% line**. The only artifact produced off the shared dev box is the cockpit's asciinema cast — `scripts/record-cockpit-cast.sh` records it on a real terminal. Note: the per-collection envelope is a pre-1.0 at-rest format change, so a `v0.2.0` encrypted store must be re-created.

## Phase 4 — Advanced / stretch features → `v0.4.0`, `v0.5.0`, … (launch is `v1.0.0`, several releases out)

Unlike the earlier phases, Phase 4 is a **backlog shipped incrementally**: each minor release (`v0.4.0`, `v0.5.0`, …) delivers a coherent, owner-gated subset, and **`v1.0.0` is reserved for the launch release** once the backlog and the launch polish below are complete. Releases ship incrementally — **`v0.4.0`**, **`v0.5.0`**, **`v0.6.0`**, **`v0.7.0`**, **`v0.8.0`**, and **`v0.9.0`** are out; we remain deliberately far from `v1.0.0`.

**Backlog (rough priority):** incremental in-place updates (SpFresh-style); migration importers (Qdrant/Chroma/pgvector); multi-vector / late-interaction scoring; optional leader-follower replication (clearly labeled); the **experimental** DCPE feature flag (published scheme only, honest caveats); then the launch polish — docs site, benchmark-table fill-in, regenerated TUI cast, published load-test results.

**`v0.4.0` ships two backlog items:** **incremental in-place index updates** — SpFresh/LIRE for **IVF**, maintained in memory so the `kill -9` crash gate is untouched by construction ([ADR-0023](adr/0023-incremental-in-place-updates.md); a durable on-disk incremental index and graph (FreshDiskANN) updates are sequenced as later increments) — and **migration importers**, `quiver admin import` loading Qdrant/Chroma/pgvector exports into Quiver collections with filterable fields ([ADR-0024](adr/0024-migration-importers.md); see [`migration.md`](migration.md)).

**`v0.5.0` ships** **HNSW incremental delete** — O(1) soft-delete with search-time tombstone filtering and an amortized rebuild ([ADR-0026](adr/0026-hnsw-incremental-delete.md)); a **neighbor-bounded IVF reassignment** that brings LIRE rebalancing to its `O(nlist + |list|)` target ([ADR-0023](adr/0023-incremental-in-place-updates.md)); a **unified secure database-open path** shared by the server, the MCP server, and the CLI so a data directory is portable between them; and the **design** for a durable on-disk incremental index ([ADR-0025](adr/0025-durable-incremental-index.md)).

**`v0.6.0` ships** the **durable on-disk incremental IVF index** — the index is snapshotted at checkpoint, referenced by the manifest under the same atomic swap as the segments, and recovered on open by loading the snapshot and replaying the WAL tail instead of an `O(N)` rebuild, with the `kill -9` crash gate extended to cover it ([ADR-0025](adr/0025-durable-incremental-index.md), Accepted) — and a **live Qdrant migration connector**: `quiver admin import --qdrant-url` pulls a running collection directly over HTTP ([ADR-0027](adr/0027-live-migration-connectors.md)). Live Chroma/Postgres connectors and leader-follower replication are sequenced for `v0.8.0`+.

**`v0.7.0` ships** **multi-vector / late-interaction (ColBERT) retrieval** — a collection can be created `multivector`, storing each document as a group of token-vector rows over the existing row-addressed store (no on-disk format change, so the `kill -9` crash gate is untouched) and ranking documents by **MaxSim** late interaction: nearest-neighbour candidate generation over the token pool, then an exact MaxSim re-rank with an optional payload filter. The token pool compresses under the same IVF+PQ / disk path, so ColBERT's storage cost showcases the memory-frugality wedge. Reachable from the embeddable database, REST + gRPC, the MCP server, and the Python/TypeScript SDKs ([ADR-0028](adr/0028-multi-vector-late-interaction.md)). Live Chroma/Postgres connectors move to `v0.8.0`.

**`v0.8.0` ships** the **live Chroma and Postgres migration connectors**, completing one-command live migration from all three supported sources: `quiver admin import --chroma-url` pulls a running Chroma collection over its v2 HTTP API (resolving the collection name to an id, then paginating `get`), and `--postgres-url` pulls pgvector rows from a running Postgres via `row_to_json` — both reusing the offline importer's normalization and write path ([ADR-0029](adr/0029-live-chroma-postgres-connectors.md)). Chroma adds no new dependency (it reuses the `ureq` seam); Postgres adds the blocking `postgres` driver with rustls/`ring` TLS, leaving the embeddable engine tokio-free. Leader-follower replication and the experimental DCPE flag are sequenced for later increments.

**`v0.9.0` ships** **asynchronous leader-follower replication** — a follower applies the leader's committed operation log (the WAL's own ops) and serves reads, lagging by the replication delay. The leader exposes an admin-scoped `Replicate` gRPC stream that ships a logical snapshot, then the live commit tail (the handler subscribes inside the same engine critical section it snapshots, so the stream is race-free with no dedup); a follower (`QUIVER_LEADER_URL`) applies the stream, serves reads, and **refuses writes**. It reuses the WAL op and the recovery apply path, so the on-disk format and the `kill -9` crash gate are untouched. Synchronous replication, failover, and consensus are explicit non-goals — single-node stays the primary topology, and this is a clearly-labelled advanced feature ([ADR-0030](adr/0030-leader-follower-replication.md); see [`replication.md`](replication.md)). The experimental DCPE flag, graph FreshDiskANN incremental updates, and the multi-vector follow-ups are sequenced for later increments.

**Per-release DoD (`v0.4.0`, `v0.5.0`, …):** the release's features are tested and documented (ADR/README/`.env.example`); coverage ≥ 80%; `just verify` and the SDK suites green; an owner-approved tag via the fast-forward release mechanic.

**Launch DoD (`v1.0.0`, several releases out):** the README benchmark table is complete and reproducible (on documented reference hardware, never fabricated); the quickstart works from a clean clone in minutes; the docs site is live; the cockpit cast is recorded; all tests/property-tests/fuzzers green; history clean and entirely the owner's. Tag **`v1.0.0`** on `main`.

## Testing posture (every phase)

Unit; `proptest` for index/storage invariants; `cargo-fuzz` for wire-protocol + on-disk parsers; crash-recovery tests; `loom` for index concurrency; recall/latency regression gates on a fixed dataset; full server+SDK integration round-trip; `criterion` microbenchmarks for SIMD. Coverage gate starts at 70% and rises to 80%+.
