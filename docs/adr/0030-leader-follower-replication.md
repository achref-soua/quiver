# ADR-0030: Leader-follower replication (async read replicas)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Achref Soua

## Context

Quiver is a single-node, single-writer engine — deliberately. The wedge is
security-first design, memory frugality, and developer experience, **not**
out-scaling distributed systems. The build brief lists leader-follower
replication as an explicitly **advanced / optional** feature, and is emphatic
that single-node excellence comes first and that we **do not promise distributed
clustering**.

A read replica still earns its place: it scales reads horizontally, keeps a warm
standby, and serves reads closer to users — without the complexity and new
failure modes of consensus. This ADR adds **asynchronous leader-follower
replication**: a follower continuously applies the leader's committed operation
log and serves reads, lagging the leader by the replication delay. Synchronous
replication, automatic failover, and consensus are explicitly **out of scope**.

The engine already has the right primitive. Every mutation is a `WalOp`
(`CreateCollection` / `DropCollection` / `Upsert` / `Delete` / `Checkpoint`)
recorded in the WAL as an LSN-sequenced `WalEntry`, and crash recovery already
**replays those ops idempotently** into the store (ADR-0005). Replication is that
same replay, sourced from the leader over the network instead of from the local
WAL. The vector and payload travel as the WAL's own opaque, pre-validated bytes.

## Decision

**1. Stream the logical operation log.** The leader exposes a gRPC
server-streaming RPC `Replicate` that yields LSN-tagged operations (the WAL's
`WalOp`). A follower applies each through the same idempotent path recovery uses,
so there is one apply implementation, not two.

**2. Bootstrap with a logical snapshot, then follow the live tail.** On connect,
the leader first streams a **logical snapshot** of current state — for each
collection a `CreateCollection` op, then one `Upsert` per live point (from
`store.scan`) — and then streams every subsequently-committed op live, from an
in-process broadcast of commits. This sidesteps WAL retention/GC entirely: a
follower never needs an op the leader has already discarded. (Resuming an
already-bootstrapped follower from its last LSN, to avoid re-streaming, is a
later optimization.)

**3. Followers are read-only.** A node in follower mode serves reads (`Search`,
`GetPoints`, `ListCollections`, …) and **rejects writes** (`Upsert`, `Delete`,
`CreateCollection`, `DropCollection`) with a clear "read-only follower" error.
Its state is owned by the replication stream, not by direct clients.

**4. Secure by default.** The `Replicate` stream is authenticated (an API key
carrying an admin-level scope) and encrypted in transit (TLS), like every other
RPC. The follower re-validates each op on apply (dim/dtype against the
descriptor, payload as JSON), so a misbehaving leader cannot inject malformed
state.

**5. Honest positioning.** Asynchronous, eventually-consistent, single-leader
read replicas — labelled **advanced / experimental**. A follower lags the leader;
reads can be stale; there is no failover and no consensus. Single-node remains
the primary, fully-supported topology.

## Implementation

Shipped across the engine and server:

- **Engine** (`quiver-core` / `quiver-embed`): a synchronous `set_commit_observer`
  hook fired after each durable commit (the tail source; a plain `Fn` keeps the
  engine runtime-agnostic), `replication_snapshot` (the bootstrap ops), and
  `apply_replicated` (a follower persists each op to its own WAL under a
  locally-assigned LSN, preserving the leader's collection id, then applies it
  through the recovery path). The `Database` reconciles its index handles on
  apply.
- **Leader** (`quiver-server`): a `Replicate` gRPC server-streaming RPC. The
  `AppState` holds a `tokio::broadcast` of committed ops, fed by the commit
  observer. The handler subscribes to the broadcast **inside the same engine
  critical section** that takes the snapshot, so no commit can interleave — the
  stream is race-free with no dedup. Admin-scoped.
- **Follower** (`quiver-server`): `leader_url` makes a node a follower; a
  background task applies the leader's stream, and a `read_only` flag refuses
  external writes. On a stream error the follower serves stale read-only state
  until an operator restarts it.

Honest deviations: the follower re-bootstraps a full snapshot on reconnect (no
incremental resume yet), and TLS to the leader is a follow-up — run replication
over a trusted network for now. Validated hermetically (an in-process leader and
follower on loopback); a multi-host deployment is an operator step.

## Consequences

- **+** Horizontal read scaling and warm standbys, with **no** change to the
  write path's latency or failure modes and no consensus machinery.
- **+** Reuses the existing op log and idempotent apply — the replication payload
  is the WAL's own `WalOp`, already a stable durability primitive — so the
  on-disk format and the `kill -9` crash gate are untouched.
- **+** Logical (not physical) streaming is codec-agnostic: leader and follower
  need not share an encryption key or on-disk layout.
- **−** A fresh or reconnecting follower re-streams a full logical snapshot (no
  incremental resume yet) — fine for a read replica, but a documented bootstrap
  cost on large collections.
- **−** Eventual consistency only: followers lag, there is no read-your-writes
  across nodes, and no failover.
- **−** The leader holds a bounded in-memory broadcast buffer; a follower that
  falls further behind than the buffer re-bootstraps from a fresh snapshot.

## Verification

Hermetic, no external services: an in-process leader and follower on loopback.
The leader creates a collection and upserts points; the follower connects,
applies the snapshot and tail, and a `Search` on the follower returns the same
results as on the leader; a later leader upsert propagates to the follower; and a
direct write to the follower is **rejected**. Validating a multi-host deployment
against real network partitions is an operator step.

## Alternatives considered

- **Synchronous / quorum replication with failover (Raft, Paxos)** — rejected for
  the 0.x line: this is exactly the distributed-clustering complexity the brief
  defers. It changes the write path's latency and failure modes and works against
  the single-node-excellence wedge.
- **Physical (page / segment / WAL-byte) replication** — rejected: it ties
  followers to the exact on-disk layout and encryption scheme. Logical op
  streaming is codec-agnostic and reuses the apply path.
- **LSN-incremental catch-up from a retained WAL** — deferred as an optimization:
  it entangles replication with WAL retention and GC. The logical-snapshot
  bootstrap is simpler and correct, at the cost of re-streaming on reconnect.
- **External file-shipping (litestream-style)** — rejected: it replicates bytes,
  not logical state, and yields a cold copy rather than a queryable, live read
  replica.
