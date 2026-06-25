# ADR-0065: Cluster mode — implementation design

- **Status:** Accepted (implementation design; takes [ADR-0051](0051-distributed-sharded-mode.md)
  from *design-only* to *built, incrementally*). Behind an opt-in `cluster`
  feature/mode; single-node stays the default with zero added overhead. Built in
  staged increments, each its own PR; increment 1 (sharding + scatter-gather, no
  consensus) lands first.
- **Date:** 2026-06-25
- **Deciders:** Achref Soua
- **Relates to:** [ADR-0006](0006-concurrency-model.md) (single writer per engine),
  [ADR-0030](0030-leader-follower-replication.md) (the per-shard replication log
  this reuses), [ADR-0005](0005-durability-and-recovery.md) (the WAL contract a
  shard must preserve), [ADR-0051](0051-distributed-sharded-mode.md) (the
  design-only shape this implements), [ADR-0057](0057-concurrent-reads-rwlock.md)
  /[ADR-0064](0064-mvcc-reads-implementation.md) (the intra-node read scaling that
  buys time before distribution).

## Context

ADR-0051 recorded the *shape* of a Quiver cluster — consistent-hash sharding, a
stateless scatter-gather router, per-shard Raft for write HA, a coordinator off
the data path — but deliberately left it unbuilt: distributed mode adds
operational weight that fights the "memory-frugal, one-binary, self-hostable"
wedge, and the rule is to be unbeatable at one node first. The single-node story
is now strong (durable disk path, off-lock rebuild, lock-free MVCC reads), and
horizontal **write** scale + beyond-one-box datasets are the remaining gap versus
Milvus/Qdrant clusters.

This ADR commits to *building* it — but only in a shape that never compromises
the single-node default, and never hand-rolls a correctness-critical primitive
where an audited one exists (the same discipline that chose `arc-swap` over
hand-rolled atomic reclamation in ADR-0064).

Two questions this ADR must settle before any code:

1. **How much to build at once?** A cluster is a router, a shard map, a
   coordinator, *and* a consensus layer. Building all of it before anything works
   is the classic distributed-systems trap.
2. **Consensus: an audited Raft crate, or a minimal in-house log?** This is the
   one subsystem where a subtle bug loses acknowledged writes or splits the
   brain — exactly the class of code the project does not hand-roll.

## Decision

**Build cluster mode in increments behind an opt-in `cluster` mode, designed for
*dynamic, elastic* scaling from the start, and use an audited Raft library for the
consensus layer rather than a hand-rolled log.**

Dynamic scaling is a first-class goal, not a bolt-on: shards must be able to
**join and leave at runtime** (manually or by an autoscaler) with **minimal data
movement** and **no downtime**. Three best-practice choices make that possible and
are baked in from increment 1:

- **Rendezvous (HRW) hashing**, not modulo-N or fixed ranges. With HRW, adding or
  removing a shard remaps only ~`1/N` of point ids — the rest stay put — so an
  elastic membership change rebalances a small, bounded slice rather than
  reshuffling the whole keyspace. (Modulo-N would remap nearly everything on a
  resize; a hash ring with virtual nodes is the alternative, but HRW gives the same
  minimal-reshuffle property with no ring bookkeeping.)
- **A versioned, refreshable shard map**, never a hard-coded static list. The map
  carries a monotonic `version`; routers cache it and **refresh** it (so a
  membership change propagates without restarting the router), and a shard that
  receives a misrouted request answers "not my range" so a stale map self-corrects
  on the data path.
- **Routing as a pure function of the *current* map**, so the same id deterministically
  resolves to its owner under whatever the live membership is.

### Sharding & routing (the data-path shape)

- **Shard map.** A versioned `ShardMap { version, shards: [{ id, url, … }] }`, keyed
  by **rendezvous (HRW) hashing of the external point id** (never the payload). It
  is **dynamic by construction** — the set can change at runtime and the map is
  refreshed, not recompiled. Increment 1 *seeds* the map from operator-declared
  shard URLs, but the type and the router are built for membership churn; online
  rebalancing (increment below) moves only the HRW-remapped slice.
- **Router.** A stateless tier that hashes the id for single-shard ops
  (upsert/get/delete) and **scatter-gathers** queries: send the ANN/hybrid query
  to every shard, request `k` (or `k·overfetch` under id-hash skew) from each,
  and merge to a global top-`k` by exact score — the same approximate-then-re-rank
  shape as a single node, one level up. Hybrid RRF fuses per shard, then the
  gather merges. The router caches the shard map; a stale map self-corrects via a
  shard-side "not my range" redirect, so the coordinator is never a per-query
  dependency.
- **Each shard is an ordinary single-writer Quiver engine** (ADR-0006), durable on
  its own disk (ADR-0005), with its own `kill -9` crash gate — **unchanged**. The
  cluster is *composition over the existing engine*, not a new engine.

### Consensus: an audited Raft crate, not in-house

For per-shard write HA (promote a replica on primary failure with **no lost
acknowledged write** and **no split-brain**), the chosen direction is **one Raft
group per shard** (per-shard, not one global group, so write scale is linear).
The implementation will adopt an **audited, maintained Raft crate** — the
candidates are `openraft` (async-native, fits the tokio server and the existing
async replication path) and `raft-rs` (the TiKV core, battle-tested but
sync/storage-centric); the increment that builds consensus picks between them
against the then-current maintenance and audit status, and the WAL stays the
replicated log the Raft layer drives (followers already apply it via ADR-0030).

**Why not hand-roll it:** consensus is the canonical example of a primitive where
a subtle bug is catastrophic and not caught by ordinary tests — the same reason
ADR-0064 used `arc-swap` instead of hand-rolled atomic-pointer reclamation. A
minimal in-house Raft is a multi-quarter correctness project with its own crash
gate; an audited crate is the lazy-and-correct choice. (A `cargo-deny`/`cargo-audit`
review of the chosen crate's dependency tree gates its adoption, as for any new
heavy dependency.)

## Increments (each its own flag-gated PR, single-node default untouched)

1. **Sharding + scatter-gather over a refreshable map.** The `ShardMap` (HRW
   hashing, versioned, refreshable — *not* a static-only list), and a router that
   single-shard-routes writes/gets and scatter-gathers queries over the current
   membership of single-primary shards (no replicas yet). Correctness oracle: a
   multi-shard cluster returns the **same top-k as a single node** holding the same
   data (scatter-gather vs single-node ground truth). The map is seeded from
   operator-declared shard URLs; the *type and router* already support membership
   churn so increment 3 adds it without a redesign. No HA yet — a shard down means
   its slice is unavailable, surfaced honestly.
2. **Per-shard read replicas.** Reuse ADR-0030 log shipping per shard so a shard's
   replicas serve reads and stay warm — read scale-out within the cluster.
3. **Dynamic, elastic membership + online rebalancing (the headline of dynamic
   scaling).** A thin **coordinator** owns the versioned shard map and shard health;
   routers **refresh** the map (no restart), and a shard added or removed at runtime
   triggers **online migration** of only the HRW-remapped `~1/N` slice — reads and
   writes continue throughout (the donor serves until the recipient is caught up,
   then the map version flips; a stale router self-corrects via the shard-side "not
   my range" redirect). This is what makes scaling *dynamic*: grow or shrink the
   cluster under live load without downtime and without reshuffling the whole
   dataset.
4. **Per-shard consensus (Raft) + automatic failover (write HA).** Adopt the chosen
   audited Raft crate; leader election, promotion-without-data-loss. Tests:
   partition/rejoin, leader-kill failover (no lost acked writes), split-brain
   safety, all against single-node ground truth; loom/property tests where they fit
   the small deterministic state machines.
5. **Autoscaling hooks (optional, best-practice automation).** Drive increment-3
   membership changes from load/capacity signals (per-shard RSS, QPS, disk) behind
   an explicit policy — so the cluster can scale itself, not only by operator action.
   Kept last and opt-in: automatic resizing is only safe once online rebalancing
   (3) and write HA (4) are proven.

Single-node remains the default and pays nothing: cluster code is behind the mode
gate, and an engine that is not a shard behaves exactly as today.

## Consequences

- **+** Linear write/capacity scale and beyond-one-box datasets, composed from the
  existing engine + replication log — not a rewrite.
- **+** Each increment is independently useful and testable against single-node
  ground truth; consensus, the hard part, is last and uses audited code.
- **−** Operational weight (router, shard map, eventually a coordinator and a
  consensus layer) that a single node does not have — strictly opt-in; the one
  binary stays the default.
- **−** Scatter-gather latency is bounded by the slowest shard; recall under
  id-hash skew needs over-fetch tuning (measured, not guessed).
- **−** A Raft dependency is a heavyweight tree to vet (`cargo-deny`/audit) and
  carry — accepted over the far larger risk of a hand-rolled consensus bug.

## Alternatives considered

- **Hand-rolled minimal Raft / in-house consensus** — rejected as the default:
  correctness-critical, multi-quarter, the exact class of primitive the project
  delegates to audited code (cf. ADR-0064). Reconsidered only if no maintained
  crate fits the WAL-as-log model.
- **Build the whole cluster before shipping anything** — rejected: the router,
  map, coordinator, and consensus landing together is untestable and high-risk.
  Increment 1 (sharding + scatter-gather) is useful and provable on its own.
- **One global Raft group** — rejected (serializes all writes; per-shard groups
  scale linearly), as in ADR-0051.
- **Range/attribute sharding as the default** — rejected (hot-spots on skewed
  keys); hash is the default, range routing may return for locality-sensitive
  filters.
- **Stay single-node** — still the right default for most deployments; this ADR
  makes the cluster *available*, not mandatory.
