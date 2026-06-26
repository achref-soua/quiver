# ADR-0067: Per-shard Raft for write high-availability (the `openraft` choice)

- **Status:** Accepted (implementation design for [ADR-0065](0065-cluster-mode-implementation.md)
  **increment 4** — write HA + automatic failover; the multi-quarter,
  correctness-critical increment). Behind the opt-in cluster mode and **opt-in per
  shard**; single-node and a non-Raft cluster are unchanged.
- **Date:** 2026-06-26
- **Deciders:** Achref Soua
- **Relates to:** [ADR-0065](0065-cluster-mode-implementation.md) (commits to "an
  audited Raft crate, not in-house", names `openraft`/`raft-rs`, and gates adoption
  on a `cargo-deny` review — settled here), [ADR-0066](0066-dynamic-cluster-membership.md)
  (the coordinator + versioned map + leader-aware routing this extends),
  [ADR-0030](0030-leader-follower-replication.md) (the per-shard op-log shipping Raft
  replaces with consensus-committed application), [ADR-0005](0005-durability-and-recovery.md)
  (the WAL + `kill -9` crash gate, per shard, that stays sacred),
  [ADR-0050](0050-snapshot-and-restore.md) (the consistent snapshot reused for Raft
  log compaction), [ADR-0006](0006-concurrency-model.md) (single writer per shard —
  now the Raft leader).

## Context

Increments 1–3 of ADR-0065 built a cluster that shards, scatter-gathers, replicates
reads, and **rebalances online** — but a shard still has a **single primary with no
write failover**: if the primary dies, that shard's slice is unavailable for writes
until an operator intervenes, and a naive promotion of a follower could lose the
last acknowledged writes or split the brain. This is the one remaining gap for
production-grade cluster durability, and it is exactly the subsystem ADR-0065
flagged as **not to hand-roll**: consensus is where a subtle bug silently loses an
acknowledged write or admits two leaders, and ordinary tests do not catch it.

ADR-0065 already decided *that* we adopt an audited Raft crate (the same discipline
that put `arc-swap` under MVCC in ADR-0064), named `openraft` and `raft-rs` as
candidates, and deferred the pick to this increment "against the then-current
maintenance and audit status", with **a `cargo-deny`/`cargo-audit` review of the
chosen crate's dependency tree gating adoption**. This ADR settles the pick with
that review, and designs the integration.

## Decision

**Adopt [`openraft`](https://crates.io/crates/openraft) for per-shard write HA — one
Raft group per shard, with the shard's existing WAL as the replicated state machine
and its read replicas as the Raft voters — opt-in per shard.**

### The crate choice — settled by the `cargo-deny` gate

A throwaway crate added each candidate at its latest crates.io release and ran the
project's `deny.toml` over the resulting tree:

| Candidate | Latest | ~Transitive deps | `cargo-deny` result |
| --- | --- | --- | --- |
| **`openraft`** | **0.9.24** | ~80 | **`advisories ok · bans ok · licenses ok · sources ok`** — clean |
| `raft-rs` (`raft`) | 0.7.0 (2022) | ~71 | **`advisories FAILED`** — pulls `protobuf 2.28.0` (RUSTSEC stack-overflow on untrusted input; fix needs `protobuf ≥ 3.7.2`, which the stale `raft-proto`/`protobuf-build` chain cannot use) |

`openraft` wins on **all three** of the criteria ADR-0065 named:

- **Audit/advisory status (the gate):** its dependency tree is `cargo-deny`-clean;
  `raft-rs`'s published crate **fails** the advisory check on `protobuf 2.28.0` and
  is effectively unmaintained on crates.io (last release 2022). A crate that cannot
  pass our existing `cargo-deny` policy cannot be adopted.
- **Maintenance:** `openraft` is actively maintained (a high, recent patch series).
- **Fit:** `openraft` is **async-native**, so it composes with the tokio server and
  the existing async replication path (ADR-0030); `raft-rs` is sync/storage-centric
  and would need a bridging layer.

The consensus core stays **the crate's** (its protocol is property-/loom-tested
upstream); we never hand-roll it.

### Integration shape (one Raft group per shard)

- **Per-shard, not global.** Each shard runs its own Raft group, so write throughput
  scales linearly with shards (a single global group would serialize all writes —
  rejected in ADR-0051/0065). The shard's **primary is the Raft leader**; its
  **replicas (increment 2) are the voters/learners**.
- **The WAL is the replicated log's state machine.** A committed Raft entry is one
  engine operation (the same `WalOp` ADR-0030 already ships and a follower already
  knows how to apply). Raft replaces "leader streams its committed tail to
  followers" with "the leader proposes, a **quorum** commits, then every member
  applies" — so a write is **acknowledged only after Raft commit**, and a failover
  can never lose an acknowledged write. The on-disk format and the per-shard
  `kill -9` crash gate (ADR-0005) are **unchanged** — Raft is a replicated log
  *above* the engine, and the engine's durability contract is untouched.
- **Automatic failover.** If the leader (primary) dies, Raft elects a new leader
  among the surviving voters with no operator action and no lost acked write; the
  losing side cannot also accept writes (no split-brain — quorum is required).
- **Leader-aware routing (extends ADR-0066).** The coordinator's versioned shard map
  gains a per-shard **leader hint**; routers route a shard's writes to its current
  leader. A write that reaches a non-leader gets a **"not the leader, here is who
  is" redirect** — the same self-correcting data-path pattern as increment 3's
  "not my range" redirect — so a stale router converges without the coordinator
  being on the write path.
- **Log compaction** reuses the consistent snapshot of ADR-0050: Raft truncates its
  log against a state-machine snapshot, and a far-behind or newly-added voter
  catches up by installing a snapshot then replaying the tail.

### Correctness model and honest scope

Our code is the **adapters around** the audited core, and that is where the bugs we
*can* introduce live, so that is what we test hardest:

- the **state-machine adapter** (apply a committed entry to the engine; produce/
  install snapshots) — deterministic, property-/`loom`-tested where the small state
  machines fit;
- the **membership wiring** (replicas as voters; add/remove a voter driven by the
  coordinator's grow/shrink);
- the **leader-aware routing** + redirect;
- **integration tests** that are the real gate: partition then rejoin; kill the
  leader under load and assert **no acknowledged write is lost**; assert **no
  split-brain** (a minority partition refuses writes); and the cluster's result
  equals **single-node ground truth** before, during, and after a failover.

This is **multi-quarter** and is built in staged increments; per ADR-0065's
direction and the build brief, **any correctness trade-off pauses for owner review**
rather than being decided unilaterally.

## Increments (staged; each its own PR, single-node default untouched)

1. **4a — adopt `openraft` + the state-machine adapter.** Add the dependency (gated
   by the `cargo-deny` review above), implement the `RaftStateMachine`/storage
   adapter over the engine's apply + snapshot paths, and run a **single-member**
   Raft group behind the cluster mode (trivially "commits" to itself) to prove the
   adapter end to end. No change to the single-node default or to a non-Raft cluster.
2. **4b — per-shard Raft group with replicas as voters + automatic failover.** Wire
   a shard's replicas as voters, route writes to the leader, commit via quorum, and
   surface the leader in the map + the not-the-leader redirect. Tests: kill-leader
   failover under load, **no lost acked write**, vs single-node ground truth.
3. **4c — log compaction + dynamic membership.** Snapshot-based log truncation
   (ADR-0050) and add/remove-voter integrated with the coordinator's grow/shrink, so
   a Raft shard participates in online rebalancing.
4. **4d — partition/split-brain hardening + the correctness gate.** Partition/rejoin,
   minority-refuses-writes, property/`loom` tests where they fit, and the full
   no-lost-acked-write / no-split-brain suite against single-node ground truth.

Raft is **opt-in per shard**: a shard configured without it stays the single-primary
shard of increments 1–3, and single-node Quiver is wholly unaffected.

## Consequences

- **+** Write **high availability**: a shard's primary can fail and a replica is
  promoted automatically with **no lost acknowledged write and no split-brain** —
  the last missing piece for production-grade cluster durability.
- **+** Reuses the engine, the WAL, the replicas (increment 2), the coordinator
  (increment 3), and the ADR-0050 snapshot — composition, and an **audited** crate
  for the one part we must not hand-roll.
- **−** A heavyweight, correctness-critical dependency (`openraft`, ~80 transitive
  crates) to carry and keep vetted — accepted because it **passes `cargo-deny`**
  today and is far safer than a hand-rolled consensus bug.
- **−** A Raft-replicated write now pays a **quorum round-trip** (consensus latency);
  this is why Raft is **opt-in per shard** — deployments that do not need write HA
  keep the cheaper single-primary shard.
- **−** Multi-quarter and the most intricate code in the cluster; mitigated by the
  audited core, the staged 4a–4d plan, and the pause-on-trade-off rule.

## Alternatives considered

- **`raft-rs`** — rejected: its published crate **fails `cargo-deny`** (the
  `protobuf 2.28.0` advisory) and is effectively unmaintained (2022); it is also
  sync-centric, a poorer fit for the async server.
- **Hand-rolled minimal Raft** — rejected (as in ADR-0065): consensus is the
  canonical primitive we delegate to audited code; a subtle bug here is catastrophic
  and not caught by ordinary tests.
- **One global Raft group** — rejected: serializes all writes; per-shard groups
  scale linearly (ADR-0051).
- **Stay single-primary (no write HA)** — the status quo after increment 3; fine for
  deployments that tolerate a manual promotion, but it leaves write HA unaddressed.
  This ADR closes the gap **opt-in**, so those deployments pay nothing.
