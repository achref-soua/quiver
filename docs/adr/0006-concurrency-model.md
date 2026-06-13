# ADR-0006: Concurrency model

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Vector search is read-dominated, with index mutation happening concurrently. We need high read concurrency without sacrificing the consistency/durability guarantees of the storage engine, and without taking on the full correctness burden of a concurrent-writer index in v1. The mechanism and tests are described in [`../concurrency/model.md`](../concurrency/model.md).

## Decision

- **Single-writer, multi-reader per collection.** One logical writer owns all mutation; many readers run queries concurrently. Collections are independent (a writer pool keyed by collection gives cross-collection write parallelism).
- **Lock-free MVCC reads.** Durable state is immutable segments + a versioned manifest behind an atomically-swappable pointer (`arc-swap`). A reader pins the current snapshot with one atomic load and runs against an immutable, repeatable view — no locks on the read path.
- **Atomic publication** of in-memory index mutations: new HNSW nodes and replaced neighbor lists are published via atomic pointer swap, so readers see pre- or post-state, never a torn one.
- **Epoch-based reclamation** (`crossbeam-epoch`) frees retired memory only after all readers that could reference it pass the epoch — no use-after-free. (Using a vetted EBR crate is justified per ADR-0001; reclamation is too subtle to hand-roll.)
- **Group commit** serialized by the single writer batches `fsync`s (ADR-0005).

## Consequences

- **+** Reads scale without blocking the writer and vice versa; consistency snapshots are trivially correct; no read-path locks.
- **−** A single per-collection writer caps single-collection ingest throughput; acceptable for v1 and recoverable via sharded writers later. Correctness depends on the atomic-publication + EBR protocol, which we model-check.

## Alternatives considered

- **Global `RwLock` per collection** — write holds block all readers and stall during long index mutations; rejected.
- **Fully lock-free concurrent-writer graph** — best throughput, heaviest correctness/verification cost; **deferred past v1**.
- **Sharded writers within a collection** — extra write parallelism but complicates single-snapshot consistency; revisit if ingest-bound.

## Verification

`loom` exhaustively model-checks the publish/consume + EBR protocol; stress tests pit N readers against 1 writer against a serial reference model; `Miri` checks the unsafe atomic/EBR paths. Part of the per-phase test gate.
