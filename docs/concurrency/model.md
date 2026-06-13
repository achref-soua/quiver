# Concurrency Model

How Quiver serves many concurrent readers while a writer mutates a collection, without locks on the read path and without losing the durability/consistency guarantees of the storage engine. The decision and alternatives are recorded in [ADR-0006](../adr/0006-concurrency-model.md); this document explains the mechanism and how it is tested.

## Shape: single-writer, multi-reader per collection

Each collection has **one logical writer** (owns all mutation: WAL append, active-segment staging, index insertion) and **many concurrent readers** (queries). Collections are independent, so a writer pool keyed by collection gives cross-collection write parallelism. This deliberately avoids concurrent in-place writers to the same index in v1 — the hardest correctness problem — while still scaling reads, which dominate a vector-search workload.

## Reads are lock-free snapshots (MVCC)

The engine's durable state is **immutable segments + a versioned manifest**. The live state is held behind an atomically-swappable pointer (`arc-swap`-style):

- A reader **pins the current state** (one atomic load → an `Arc` to {manifest version, segment set, index handles}). It then runs entirely against that immutable snapshot — no locks, no blocking, repeatable reads for the life of the query.
- The writer, after sealing a segment / compacting / checkpointing, **publishes a new state** by building the next immutable snapshot and atomically swapping the pointer. In-flight readers keep using the old snapshot; new readers see the new one.

This is MVCC: a query sees a single consistent version of the collection regardless of concurrent writes.

## The one mutable shared structure: the active segment + live index

Durable segments are immutable, but the **in-memory active segment** and the **live index graph** are mutated in place by the writer while readers traverse them. Two mechanisms keep that safe:

1. **Atomic publication of node state.** When the writer links a new HNSW node or replaces a node's neighbor list, it publishes the updated adjacency via an **atomic pointer swap**, not an in-place edit. A reader therefore observes either the pre-link or the post-link adjacency — never a half-written list. New nodes become reachable only after their adjacency is fully built and published.
2. **Epoch-based reclamation (EBR).** Memory retired by the writer (an old adjacency array, a compacted-away segment, a superseded index node) is **not freed until every reader that could hold a reference has advanced past the epoch in which it was retired**. We use a vetted EBR implementation (`crossbeam-epoch`) — reclamation is subtle enough that a hand-rolled scheme would be a needless risk (this is squarely in the "use a vetted crate" category of ADR-0001, not the "build from scratch" core). Readers enter a short epoch-pinned critical section around graph traversal.

Net effect: readers never take a lock on the hot path, the writer never waits on readers, and there is no use-after-free.

## Durability interaction

The single writer serializes WAL appends; **group commit** batches multiple upserts into one `fsync` to amortize the durability cost (window/size configurable — see ADR-0005). Readers never touch the WAL. Publication of a new snapshot happens only after the corresponding records are durable, so a reader can never observe a write that would be lost on crash.

## CPU scheduling

Per ADR-0002, the engine is synchronous; the async server offloads queries and index builds to a CPU pool (`spawn_blocking` / `rayon`) so a long search never stalls request acceptance. Index construction parallelizes *across* segments; within a single HNSW graph, v1 builds with the one writer (concurrent multi-writer graph construction is a deferred optimization).

## Why not the alternatives (see ADR-0006)

- **A global `RwLock` per collection** — simple, but write holds block all readers; unacceptable for a read-dominated store and for long index mutations.
- **A fully lock-free concurrent graph (concurrent inserts + reads)** — maximal throughput but a serious correctness/verification burden; deferred past v1.
- **Sharded writers within one collection** — extra parallelism, but complicates the single-snapshot consistency story; revisit if single-writer ingestion becomes the bottleneck.

## How this is verified

- **`loom`** model-checks the publish/consume protocol: a reader pinning a snapshot and an adjacency swap, plus EBR retirement, are exhaustively explored for data races, use-after-free, and visibility violations (a read must observe either the pre- or post-insert state, never a torn one).
- **Stress tests** run N reader threads against 1 writer thread on a real collection under randomized operations, asserting query results stay consistent with a serial reference model.
- **Miri** runs the `core`/`index` unsafe-bearing tests to catch UB in the atomic/EBR code.
- These are part of the test posture that gates each phase ([`../roadmap.md`](../roadmap.md)).
