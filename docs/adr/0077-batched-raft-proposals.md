# ADR-0077: Pipelined (batched) Raft proposals

- **Status:** Accepted
- **Date:** 2026-07-24
- **Deciders:** Achref Soua

## Context

Under per-shard Raft ([ADR-0067](0067-per-shard-raft-write-ha.md)) a client write
is proposed through the shard's Raft group and acknowledged only after quorum
commit. A **bulk** write (`points:bulk`, multi-delete) prepares many `WalOp`s and
proposes them via `AppState::raft_propose_all`.

That method looped: `for op in ops { raft.client_write(op).await?; }` — it awaited
each op's **quorum commit** before proposing the next. So an `N`-op bulk write
paid `N` sequential consensus rounds: on a real cluster, `N` full leader→followers→
ack **network round-trips**, strictly serialized. That is the classic un-pipelined
Raft client anti-pattern; throughput collapses to `1 / round-trip-latency`.

openraft is built to absorb concurrent proposals: `RaftLogStorage::append` takes a
`Vec<Entry>` and flushes it under **one** `fsync` (our `DurableLogStore::append_entries`
writes one `Record::Append(entries)` per call), and the leader replicates whatever
is pending in one `AppendEntries`. The serial await defeated all of it by never
letting more than one entry be pending.

## Decision

Pipeline the proposals with a **bounded in-flight** window instead of awaiting each
commit in turn:

```rust
futures_util::stream::iter(ops)
    .map(|op| self.raft_propose(rs, op))
    .buffered(RAFT_PROPOSE_MAX_INFLIGHT) // 1024
```

**Contract (unchanged durability & linearizability):**

- **Order preserved.** `buffered` first-polls its futures in stream order, and
  `client_write` sends to openraft's `RaftCore` mpsc on first poll, so entries are
  accepted — and log indices assigned — in submission order. Submission order =
  log order = apply order, so linearizability is exactly as before.
- **Per-op durability.** Each op is still individually appended, `fsync`ed, and
  quorum-committed before *its* future resolves; the ack semantics are unchanged.
  Pipelining only lets openraft coalesce the *pending* entries into one log append
  (one `fsync`) and one replication round.
- **Bounded.** At most `RAFT_PROPOSE_MAX_INFLIGHT` (1024) proposals are in flight,
  capping memory/backpressure on a large bulk request. This is effectively the
  batch size; openraft coalesces within it. *(ponytail: a fixed bound, not a config
  knob — no workload has asked to tune it.)*
- **Honest partial failure.** Every submitted op's result is **drained** — never
  cancelled mid-flight — and the first error is surfaced. A mid-batch leadership
  change fails the ordered suffix; because apply is idempotent (deterministic
  upsert/delete, `Database::apply_replicated`), retrying the whole batch is safe.
  This matches the pre-existing non-atomic per-op behaviour (no regression).
- **No on-disk change.** `D` is still `WalOp`, one op per log entry; the durable
  log format and the `kill -9` crash gate are untouched.

## Consequences

- **Win — network-RTT amortization (the point).** On a multi-node cluster the `N`
  per-op round-trips overlap into a pipeline, so bulk-write throughput stops being
  latency-bound. This is the lever for `points:bulk` under Raft.
- **Measurement, honest.** The available harness is a **single-member** group on
  one box, where there are *no* network round-trips to overlap and the path is
  `fsync`-bound (openraft does not batch the append `fsync` across pending
  single-member writes). Measured there (2000 upserts, durable log):
  serial 36 ops/s vs pipelined 38 ops/s ≈ **1.1×** — within noise, as expected.
  **The cluster-throughput number is deferred to real multi-node hardware and is
  never fabricated or extrapolated** (same discipline as the 100M/RSS deferrals).
  The ignored `bench_batched_vs_serial_proposals` records the single-box figure and
  proves the pipelined path commits every op.
- **Risk — low.** Behaviour-preserving except for the drain-all-on-error (which is
  strictly more honest — no submitted op's outcome is dropped). No new dependency
  (`futures-util` was already a dep).

## Alternatives considered

- **One log entry per client batch (`D = Vec<WalOp>` / a `LogBatch` newtype).** True
  "single append round": `N` ops become one append + one `fsync` + one commit + one
  apply, which *would* also show a win on the single-box `fsync`-bound harness, and
  makes a bulk write **atomic**. Rejected for now: it is a **durable Raft-log format
  change** (existing logs need a fail-loud version gate to avoid silently dropping a
  tail on upgrade) and shifts bulk-write semantics to all-or-nothing. Sequenced as
  an owner-gated follow-up, not built speculatively — the pipelining win is free and
  format-stable, and the batch-entry win is primarily redundant with it on a real
  cluster (where RTT, not `fsync`, dominates).
- **A linger timer to accumulate cross-request batches.** Unneeded — a bulk request
  already arrives as one op vector, which *is* the batch. No timer machinery.
