# Architecture Decision Records

Every significant, hard-to-reverse decision is captured here as a short, numbered record so future readers understand *why* the system is shaped the way it is.

## Format

Each ADR is `NNNN-kebab-title.md` with:

- **Status** — Proposed · Accepted · Superseded by ADR-XXXX · Deprecated
- **Date** and **Deciders**
- **Context** — the forces and constraints in play
- **Decision** — what we will do
- **Consequences** — the trade-offs we accept, good and bad
- **Alternatives considered** — what we rejected and why

ADRs are immutable once Accepted; we supersede rather than edit. Numbers are stable and never reused.

## Index

| # | Title | Status | Phase |
|---|---|---|---|
| [0001](0001-language-and-workspace.md) | Language and workspace layout | Accepted | 0 |
| [0002](0002-async-runtime.md) | Async runtime — Tokio | Accepted | 0 |
| [0003](0003-serialization.md) | Serialization formats | Accepted | 0 |
| [0004](0004-on-disk-format.md) | On-disk format | Accepted | 0 |
| [0005](0005-durability-and-recovery.md) | Durability & crash recovery | Accepted | 0 |
| [0006](0006-concurrency-model.md) | Concurrency model | Accepted | 0 |
| [0007](0007-index-roadmap.md) | Index roadmap (HNSW → Vamana/IVF) | Accepted | 0 |
| [0008](0008-quantization.md) | Quantization strategy | Accepted | 0 |
| [0009](0009-simd-kernels.md) | SIMD distance kernels | Accepted | 0 |
| [0010](0010-crypto-envelope-aead.md) | Crypto: envelope encryption & AEAD | Accepted | 0 |
| [0011](0011-authn-authz-tenancy.md) | AuthN/Z & tenant isolation | Accepted | 0 |
| [0012](0012-client-side-encryption.md) | Client-side encryption & trust boundary | Accepted | 0 |
| [0013](0013-config-and-secure-defaults.md) | Configuration & secure defaults | Accepted | 0 |
| [0014](0014-observability.md) | Observability | Accepted | 0 |
| [0015](0015-ci-policy.md) | CI policy — manual-only + local verify gate | Accepted | 0 |
| [0016](0016-license-agpl.md) | License — AGPL-3.0 | Accepted | 0 |
| [0017](0017-error-handling.md) | Error handling | Accepted | 0 |
| [0018](0018-sdk-and-integration-strategy.md) | SDK & integration strategy | Accepted | 0 |
| [0019](0019-disk-index-format.md) | Disk-resident index format (DiskANN on encrypted pages) | Accepted | 2 |
| [0020](0020-row-addressed-segment-storage.md) | Row-addressed segment storage (`.vec`/`.pay`/`.dir`, mmap) | Accepted | 2 |
| [0021](0021-tombstones-and-compaction.md) | Tombstones (roaring `.del`) and compaction | Accepted | 2 |
| [0022](0022-secondary-indexes.md) | Secondary indexes (`.sec`, order-preserving keys) | Accepted | 2 |
| [0023](0023-incremental-in-place-updates.md) | Incremental in-place index updates (SpFresh / LIRE) | Accepted | 4 |
| [0024](0024-migration-importers.md) | Migration importers (Qdrant / Chroma / pgvector) | Accepted | 4 |
| [0025](0025-durable-incremental-index.md) | Durable on-disk incremental index (IVF) | Accepted | 4 |
| [0026](0026-hnsw-incremental-delete.md) | HNSW incremental delete (soft-delete) | Accepted | 4 |
| [0027](0027-live-migration-connectors.md) | Live migration connectors (Qdrant over HTTP) | Accepted | 4 |
| [0028](0028-multi-vector-late-interaction.md) | Multi-vector documents & late interaction (ColBERT / MaxSim) | Accepted | 4 |
| [0029](0029-live-chroma-postgres-connectors.md) | Live Chroma & Postgres migration connectors | Accepted | 4 |
| [0030](0030-leader-follower-replication.md) | Leader-follower replication (async read replicas) | Accepted | 4 |
| [0031](0031-dcpe-vector-encryption.md) | Experimental property-preserving vector encryption (DCPE) | Accepted | 4 |
| [0032](0032-client-side-vector-encryption.md) | Semantically secure client-side vector encryption (opaque vectors) | Accepted | 4 |
| [0033](0033-graph-incremental-freshdiskann.md) | Graph-index incremental updates (FreshDiskANN StreamingMerge) | Accepted | 4 |
| [0034](0034-multivector-followups.md) | Multi-vector follow-ups (incremental maintenance, native rows, ColBERTv2/PLAID) | Accepted | 4 |
| [0035](0035-docs-site-and-dcpe-hardening.md) | Documentation site (mdBook), DCPE hardening (shuffle + normalisation), native TS cipher | Accepted | 4 |
| [0036](0036-retro-cockpit-design-system.md) | Retro cockpit design system (Bronze Quiver theme, logo, decoration vocabulary) | Accepted | 4 |

| [0037](0037-scientific-multi-db-benchmark-suite.md) | Scientific multi-DB benchmark suite | Accepted | 5 |
| [0038](0038-batch-wal-upsert.md) | Batch WAL sync for upsert (build-time bottleneck fix) | Accepted | 5 |
| [0039](0039-one-command-install.md) | One-command install and self-update (`quiver update`) | Proposed | 5 |
| [0040](0040-query-cost-limits.md) | Query cost limits (caps on `k`, `ef_search`, dimension, payload, batch) | Accepted | 5 |
| [0041](0041-deep-benchmark.md) | Deep, large-data benchmark dimensions (SIFT1M, concurrency, Pareto, quantization curve) | Accepted | 5 |
| [0042](0042-rag-ergonomics.md) | RAG/agentic ergonomics (async SDK, Haystack, MCP introspection) + usage docs | Proposed | 5 |
| [0043](0043-hybrid-sparse-search.md) | Hybrid (dense + sparse) search with RRF fusion | Proposed | 5 |
| [0044](0044-automated-release-assets.md) | Automated, tag-triggered multi-platform release assets (Windows job added) | Accepted | 5 |
| [0045](0045-hybrid-everywhere-and-fast-ingest.md) | Hybrid everywhere + fast ingest (sparse inverted index, gRPC/MCP/TS parity, bulk upsert) | Accepted | 5 |
| [0046](0046-bm25-full-text.md) | BM25 / full-text over the sparse path (tokenizer + BM25 scoring, `text`/`query_text`) | Accepted | 5 |
| [0047](0047-server-side-embedding-and-rerank-hooks.md) | Server-side embedding & reranking hooks (provider-agnostic, opt-in per collection) | Accepted | 5 |
| [0048](0048-snowball-stemmer.md) | Snowball (Porter2) stemmer for BM25 tokenization | Accepted | 5 |
| [0049](0049-per-key-rate-limiting.md) | Per-key rate limiting (token bucket, RateLimit headers, 429) | Accepted | 5 |
| [0050](0050-snapshot-and-restore.md) | Online snapshot & restore (consistent whole-dir copy, REST + MCP) | Accepted | 5 |
| [0051](0051-distributed-sharded-mode.md) | Distributed / sharded mode (hash sharding, scatter-gather, per-shard Raft) — design only | Proposed | 5 |
| [0052](0052-gpu-acceleration.md) | GPU-accelerated build & search (behind the index trait, feature-gated) — design only | Proposed | 5 |
| [0053](0053-lock-free-mvcc-reads.md) | Lock-free MVCC reads (versioned snapshots, epoch reclamation) — design only | Proposed | 5 |
| [0054](0054-prometheus-metrics-and-tracing.md) | Prometheus `/metrics` (real counters/histograms) + request tracing spans + Grafana dashboard | Accepted | 5 |
| [0055](0055-benchmark-v0.20.0-bulk-build.md) | v0.20.0 multi-DB benchmark re-run with the bulk-ingest build path (honest time-until-queryable) | Accepted | 5 |
| [0056](0056-packaging-and-distribution.md) | Packaging & distribution — publish pipeline (crates.io/PyPI/npm), Helm chart, CHANGELOG | Accepted | 5 |
| [0057](0057-concurrent-reads-rwlock.md) | Concurrent reads behind a reader–writer lock + `&self` snapshot reads (staged path to lock-free arc-swap) | Accepted | 4 |
| [0058](0058-mcp-text-tools-and-provider-crate.md) | MCP `upsert_text`/`search_text` tools + extract the embedding/rerank seam into the shared `quiver-providers` crate | Accepted | 4 |

Phase-0 ADRs (0001–0018) are Accepted; Phase-2 decisions span 0019–0022; Phase-4 decisions begin at 0023. New decisions take the next free number; superseded ADRs are marked as such — never deleted or renumbered.
