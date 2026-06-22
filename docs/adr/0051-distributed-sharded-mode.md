# ADR-0051: Distributed / sharded mode (design only)

- **Status:** Proposed (design only — not implemented; gated on explicit owner go-ahead)
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

Quiver is a single-node engine: one writer behind a mutex (ADR-0006), durable
on local disk (ADR-0004/0005), with optional async read replicas via
leader-follower replication (ADR-0030). This scales reads horizontally and a
single node already serves tens of millions of vectors on the disk path, but it
caps **write throughput** at one node and **dataset size** at one machine's
disk. The biggest single gap versus Milvus and Qdrant clusters is horizontal
**write** scale and billion-scale datasets that exceed one box.

This ADR records the intended design so the path is clear, *without committing
to build it*. The honest position (ADR roadmap, Tier 4 #17): distributed mode is
a non-goal until the single-node story is unbeatable, because a premature
cluster adds operational weight that contradicts the "memory-frugal,
self-hostable, one-binary" wedge.

## Decision (intended design)

Introduce sharding by **consistent hashing of the external point id** into N
shards, each shard an independent single-writer Quiver engine, fronted by a
stateless **router**:

- **Shard map.** A versioned `ShardMap { version, shards: [{id, range, primary,
  replicas}] }` held in a small coordination store. Start with a static map
  (operator-declared N) and add online resharding later; never hash on payload.
- **Routing.** The router hashes the point id for writes/gets (single shard) and
  **scatter-gathers** queries: send the ANN query to every shard's primary,
  merge the top-k by score, return the global top-k. `k` is requested from each
  shard at `k` (or `k·overfetch` for recall under skew); hybrid/RRF fusion
  happens after the gather, per shard then merged.
- **Replication & HA.** Reuse ADR-0030's log shipping per shard for read
  replicas. For **write HA** (primary failover without data loss) the shard's
  WAL is the replication primitive; promoting a follower requires a consensus
  layer so promotion is agreed and split-brain is impossible — **Raft over the
  per-shard WAL** is the chosen direction (the WAL *is* the replicated log;
  followers already apply it). One Raft group per shard, not one global group,
  so write scale is linear in shard count.
- **Coordinator.** A thin control plane owns the shard map, membership, health,
  and reshard/rebalance orchestration. It is **off the data path** (routers
  cache the map; a stale map self-corrects via a shard-side "not my range"
  redirect), so the coordinator is not a per-query dependency.
- **Consistency.** Per-shard linearizable writes (single writer); cross-shard
  queries are **eventually consistent** snapshots (no global transaction) —
  acceptable for vector search, and the only scalable option without a global
  clock.

## Consequences

- **+** Linear write and capacity scale; billion-scale datasets; per-shard HA
  with no data loss on failover.
- **+** Reuses the existing engine and replication log unchanged per shard —
  the cluster is composition, not a rewrite.
- **−** Operational weight (a coordinator, a consensus layer, a router tier)
  that a single self-hosted node does not have — directly in tension with the
  wedge. Must stay strictly opt-in; the single binary must remain the default.
- **−** Scatter-gather query latency is bounded by the slowest shard; recall
  under id-hash skew needs over-fetch tuning.
- **−** Raft per shard is a substantial, correctness-critical subsystem with its
  own crash-safety gate — a multi-quarter effort, not a sprint.

## Alternatives considered

- **Range/attribute sharding** instead of hash — rejected as the default: it
  hot-spots on skewed key distributions; hash gives even load. (Range routing
  may return as an option for locality-sensitive payload filters.)
- **One global Raft group** — rejected: serializes all writes through one
  leader, defeating write-scale; per-shard groups scale linearly.
- **External orchestration (Vitess/Citus-style proxy) only, no consensus** —
  rejected for write HA: failover without consensus risks split-brain and lost
  acknowledged writes, violating the ADR-0005 durability contract.
- **Do nothing / stay single-node** — the *current* decision. Revisit only once
  the single-node benchmark + memory wedge are demonstrably best-in-class and a
  concrete user need for billion-scale write throughput exists.
