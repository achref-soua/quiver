# ADR-0038 — Batch WAL sync for upsert to fix build-time bottleneck

**Status:** Proposed
**Date:** 2026-06-18
**Deciders:** Achref Soua

---

## Context

Phase B (ADR-0037) measured Quiver's build time against seven competitors on SIFTSMALL (10k vectors, 128-d, L2):

| Competitor | Build (s) |
|---|---|
| FAISS 1.14.3 | 0.2 |
| Qdrant 1.13.4 | 0.9 |
| Milvus Lite 3.0.0 | 0.8 |
| Chroma 1.5.9 | 1.2 |
| pgvector 0.7/pg16 | 1.9 |
| Weaviate 1.27.0 | 24.1 |
| **Quiver v0.17.0-dev** | **65.4** |

Quiver loses the build-time comparison decisively (32–327× behind FAISS–Milvus), while leading on every other axis: lowest RSS (61 MB), highest QPS vs Docker-wrapped competitors (1233 vs next-best 1156 for pgvector), and competitive recall.

### Root-cause profiling

An isolated profiling run on the dev box (2026-06-18) decomposed the 65.4s into two components by comparing a normal HNSW collection against a `client_side`-encrypted collection (same REST path, same WAL writes, but no HNSW index maintenance):

| Component | Time | Share |
|---|---|---|
| WAL + store (no HNSW) | 64.53s | **99%** |
| HNSW insertions only | 0.95s | 1% |

The HNSW algorithm itself is fast (< 1s for 10k vectors at dim=128 with `efConstruction=200`). **All 65 seconds are WAL `fdatasync` overhead.**

#### Why: one `fdatasync` per point

`Store::upsert()` calls `WalWriter::append_sync()`, which is `append()` followed immediately by `sync()` (`fdatasync`). Each REST `upsert` batch (500 points) runs 500 sequential fsyncs. For 10k points: 10,000 fsyncs × ~6.45ms/fsync (WSL2) = ~64.5s.

WSL2 is known to have elevated fsync latency, but the pattern — one fsync per individual point — is inefficient on any storage backend. The correct unit of durability is the *batch*, not the individual point.

---

## Decision

Add `Store::upsert_batch()` in `quiver-core` and `Database::upsert_batch()` in `quiver-embed`, then wire the server's `AppState::upsert()` to use the batch path. The batch variant:

1. Validates all vectors against the collection's expected dimensionality.
2. Builds one `WalEntry` per point and calls `WalWriter::append()` (no sync) for each.
3. Calls `WalWriter::sync()` **once** for the entire batch.
4. Publishes all entries in order (replication observer, ADR-0030).
5. Applies in-memory state for all entries in order.
6. Calls `index_upsert_point()` per point to maintain the HNSW / IVF / graph index.

The existing per-point `Store::upsert()` is kept for single-point callers (gRPC path, MCP `upsert` tool, replication apply) that already hold their own WAL-sync boundary.

### Why this is correct

The WAL guarantees durability at the *commit* granularity. A commit is "acknowledged after fsync." Batching multiple appends under one fsync changes the commit granularity from per-point to per-batch — exactly the model used by every production database system (Postgres `synchronous_commit`, SQLite WAL mode, RocksDB write batches).

**Durability impact:** If the server crashes mid-batch (after some appends but before the fsync), none of the batch's points are durable — the in-memory state was not yet updated. The client receives no success response; it retries the whole batch. This is strictly correct: the client can never observe a partial batch as committed, because the REST response is only sent after the fsync.

**Before this change:** A crash after the k-th individual fsync (but before the REST response) could leave k points durable in the WAL while the client retried the whole batch. The new behavior is simpler and more predictable: a batch is atomic.

**Recovery:** WAL replay (`apply_wal_entry`) processes individual `WalOp::Upsert` records — one per point, same as before. The WAL format is unchanged; only the timing of the fsync changes.

**Replication (ADR-0030):** `publish()` is called per entry *after* the single fsync, preserving the invariant that the observer is only notified of durable commits.

---

## Alternatives considered

### A. Keep per-point sync, reduce fsync cost via O_DIRECT + io_uring

Would require deep platform-specific I/O plumbing (Linux-only, async complexity, `io_uring` feature flag). Disproportionate to the problem. Rejected.

### B. Coalesce fsyncs in a background group-commit thread

Standard in high-throughput write systems (MySQL binlog group commit). Adds latency uncertainty and implementation complexity. Overkill for Quiver's single-node REST model where the batch boundary is already defined by the HTTP request. Rejected for now.

### C. Per-batch fsync (chosen)

Simplest change, fits naturally into the existing REST handler which already processes a whole batch in one `run_blocking` closure. Zero new dependencies. Zero format changes.

---

## Implementation

**Files changed:**
- `crates/quiver-core/src/store.rs` — `Store::upsert_batch()`
- `crates/quiver-embed/src/lib.rs` — `Database::upsert_batch()`
- `crates/quiver-server/src/lib.rs` — `AppState::upsert()` → batch path

**Tests added:**
- `quiver-core` unit test: `upsert_batch_commits_all_on_sync` — inserts N points via batch, reopens the store, confirms all N are readable (WAL replay correct).
- `quiver-embed` unit test: `upsert_batch_index_consistent_with_sequential_upserts` — batch result equals sequential-upsert result for HNSW search.
- Existing `just verify` suite (round-trip, crash-recovery, acceptance) covers correctness end-to-end.

---

## Consequences

**Positive:**
- Build time drops from ~65s to ~2s for 10k SIFTSMALL vectors (expected: 20 fsyncs × 6.45ms = 0.13s WAL + 0.95s HNSW = ~1.1s). Measured before/after numbers will be committed with the implementation PR.
- Quiver becomes competitive on build time: closer to Qdrant (0.9s) and faster than Weaviate (24.1s).
- RSS, QPS, and recall are unaffected (no algorithm change).

**Negative / risks:**
- Batch atomicity (whole batch or nothing) is a slight semantic tightening vs the previous per-point durability. Acceptable — no client can observe partial batches anyway (REST response comes only after the batch commits).
- Existing per-point gRPC / MCP upsert paths are unchanged (still use `Store::upsert()`); their performance is unchanged.

---

## Implementation status

- [ ] `Store::upsert_batch()` + unit test
- [ ] `Database::upsert_batch()` + unit test
- [ ] `AppState::upsert()` wired to batch path
- [ ] `just verify` green
- [ ] Before/after numbers committed to `docs/benchmarks/results/comparison-v0.17.0/smoke/quiver.csv`
- [ ] ADR status → Accepted
