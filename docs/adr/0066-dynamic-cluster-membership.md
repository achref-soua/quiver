# ADR-0066: Dynamic cluster membership, online rebalancing, and the coordinator

- **Status:** Accepted (implementation design for [ADR-0065](0065-cluster-mode-implementation.md)
  increment 3 — the headline of *dynamic, elastic* scaling). Built in staged
  sub-increments behind the same opt-in cluster mode; single-node and a statically
  configured cluster stay unaffected.
- **Date:** 2026-06-25
- **Deciders:** Achref Soua
- **Relates to:** [ADR-0065](0065-cluster-mode-implementation.md) (the cluster
  increments this continues — sharding + scatter-gather in increment 1, read
  replicas in increment 2), [ADR-0051](0051-distributed-sharded-mode.md) (the
  design-only shape: a coordinator off the data path, routers that cache the map,
  a shard-side redirect on a stale map), [ADR-0030](0030-leader-follower-replication.md)
  (the per-shard log-shipping reused to copy a migrating slice), [ADR-0006](0006-concurrency-model.md)
  (single writer per shard — the invariant a migration must never break).

## Context

Increments 1 and 2 of ADR-0065 run over a shard map that is **seeded once at
startup** from `QUIVER_CLUSTER_SHARDS` / `QUIVER_CLUSTER_REPLICAS`. That already
shards writes (HRW) and scatter-gathers searches, and replicas scale reads — but
the membership is fixed: changing the shard set means restarting every router, and
there is no way to grow or shrink the cluster under live load.

Increment 3 is what makes scaling *dynamic*: shards must **join and leave at
runtime** (by an operator or, later, an autoscaler) while

1. **only the HRW-remapped `~1/N` slice moves** — survivors keep their data,
2. **nothing goes down** — the moving slice stays readable and writable throughout,
   and
3. **no acknowledged write is lost** — a write accepted before, during, or after a
   migration is still there after it.

ADR-0065 deliberately built increments 1–2 so this lands *without a redesign*: the
router already holds its map behind an `ArcSwap` (refreshable), and HRW already
gives minimal reshuffle. Three gaps remain, and they are exactly what this ADR
settles before any code:

- **A stable shard identity.** Today the HRW seed is the shard's *position* in the
  list (`Shard.index`). Removing a shard and re-packing positions would re-key every
  survivor and move *all* the data — the opposite of the HRW property. Dynamic
  membership needs an **immutable id** decoupled from position.
- **An authority for the map.** Routers need somewhere to *refresh* the current
  membership and version from, without a restart.
- **A migration protocol that is correct.** Copying a slice from a donor to a
  recipient while both are live, then transferring ownership atomically, with no lost
  writes and no split ownership, is the one genuinely hard, correctness-critical part.

## Decision

**Add a thin, off-the-data-path coordinator that owns a monotonically versioned
shard map; give each shard an immutable id (the HRW key); and migrate a slice
online with a donor-serves-until-caught-up handshake whose ownership transfers at a
single version flip, made self-correcting by a shard-side "not my range" redirect.**

### 1. Stable shard ids (the HRW key)

`Shard` carries an immutable `id` (a `u64`, assigned at join and **never reused**),
and HRW hashes over `id` rather than list position. The `ShardMap` becomes a *set*
of shards keyed by id, tolerating gaps — removing a shard drops its id and leaves
every survivor's id (and therefore its data) untouched, so only the removed shard's
`~1/N` slice remaps. `shard_for` and `partition` key by id, not `Vec` position.
`from_urls` stays working by assigning stable ids deterministically (the first build
freezes them); single-node and a statically configured cluster are unaffected. This
is a backward-compatible refactor of the `quiver-cluster` crate — pure, no I/O — and
keeps the pinned FNV-1a value and the HRW distribution/minimal-reshuffle tests.

### 2. The coordinator (off the data path)

A thin service — `quiver serve` in a coordinator mode (`QUIVER_COORDINATOR_*`) —
owns the authoritative **versioned shard map**:

```text
ShardMapV { version: u64, shards: [{ id, primary_url, replica_urls, state }] }
state ∈ { active, joining, draining }
```

and per-shard health (heartbeats). It exposes a small REST API:

- `GET /cluster/map` — the current versioned map (cheap, cacheable; the router's
  refresh source),
- `POST /cluster/shards` / `DELETE /cluster/shards/{id}` — add / drain-and-remove a
  shard (operator or, later, the autoscaler),
- `GET /cluster/health` — per-shard liveness for operability.

The coordinator is **not on the query path**: routers cache the map and refresh it
on an interval (and on a redirect, below), so the coordinator being briefly
unavailable stops *membership changes*, not *serving*. It is **single-node in this
increment** — its state is operator intent plus shard-derived health, persisted to
its own disk so a restart recovers; coordinator HA can later ride increment 4's
Raft. This limit is stated honestly, not hidden: a cluster keeps serving reads and
writes with the coordinator down; it just cannot resize until it is back.

### 3. Router refresh (no restart)

The router polls `GET /cluster/map` on an interval and swaps any newer-`version` map
into its existing `ArcSwap<ShardMap>` (already present since increment 1 — no
restart, no torn reads). A stale response (`version` ≤ current) is ignored. With no
coordinator configured, the router uses its static `QUIVER_CLUSTER_SHARDS` map
exactly as in increments 1–2 — dynamic membership is strictly additive and opt-in.

### 4. Online migration — the state machine

Adding shard `S` (or removing one) changes ownership of a **slice** = the ids that
hash to a different shard under the new membership. Migration is a coordinator-driven,
versioned handshake. For an add:

- **`v → v+1` — mark `S` joining; donor still owns the slice.** The coordinator
  publishes the map with `S` present but the moved slice **still owned by its
  donor(s)**. The donor remains the single writer and the read source for every id
  in the slice; `S` is only *catching up*. (The single-writer-per-id invariant of
  ADR-0006 is never violated: exactly one shard owns a given id under the committed
  version.)
- **Bulk copy + live tail (reuse ADR-0030).** `S` pulls the slice from each donor by
  **reusing the leader-follower replication stream**, scoped to the slice: a logical
  snapshot of the slice's points, then the live tail. While `S` catches up the donor
  keeps applying (and shipping) the slice's new writes, so `S` converges without the
  slice ever going dark.
- **`v+1 → v+2` — the flip.** Once `S` reports caught up (slice lag ≤ a threshold),
  the coordinator bumps the version, transferring ownership of the slice to `S`
  **atomically at that version**. The donor enters a short **grace period** still
  answering slice reads (for routers that have not refreshed yet), then **drops** the
  slice.
- **Self-correcting data path — the "not my range" redirect.** A shard that receives
  a request for an id it does not (yet, or any longer) own answers a small
  **redirect** ("not my range — refresh your map") instead of a wrong or empty
  answer. A stale router refetches the map and retries. This is why the coordinator
  is **not** a per-query dependency: the data path corrects itself across the window
  where routers disagree on the version.

**Removal / drain** is the mirror: mark the leaving shard `draining`, migrate its
whole keyspace to the HRW-next owners (by HRW each survivor picks up a fraction of
that one shard's `~1/N`), flip, then retire it.

**Why no acknowledged write is lost.** A write for a slice id is always accepted by
the shard that *owns* it under the **writing router's committed map version**:

- a stale router routing to the **donor** is correct — the donor still owns the
  slice until the flip and either retains the write (pre-flip) or has shipped it to
  `S` via the live tail;
- a router routing to `S` **before** the flip gets a redirect, refreshes, and lands
  on the donor;
- after the flip, the donor's grace window plus `S`'s tail-catch-up guarantee the
  donor's last accepted writes reach `S` **before** the donor drops the slice.

Ownership transfers at exactly one version boundary, so at every instant a given id
has a single owner — there is no window of split ownership and no double-write.

## Increments (sub-PRs for ADR-0065 increment 3)

1. **3a — stable shard ids.** Decouple the HRW seed from list position in
   `quiver-cluster`; tolerate gaps; key `shard_for` / `partition` by id. Tests:
   removing a *middle* shard moves only its slice and survivors keep theirs;
   appending still moves only `~1/N`; pinned FNV-1a and distribution tests intact.
2. **3b — the coordinator + versioned map + router refresh.** The coordinator
   service, the `GET /cluster/map` refresh, the `ArcSwap` swap on a newer version,
   and the "not my range" redirect plumbing — *without* migration yet (a freshly
   added shard owns only the keys that now hash to it; pre-existing data is moved in
   3c). Tests: a router picks up an added/removed shard with no restart; version
   monotonicity; a redirect triggers a refresh-and-retry.
3. **3c — online migration (the headline, correctness-critical).** The slice
   snapshot + live-tail copy, the donor grace window, and the version flip. Tests:
   add and remove a shard under concurrent read+write load; assert **every
   acknowledged write is later readable**, the **moved slice is queryable
   throughout**, and the cluster result matches single-node ground truth before,
   during, and after — property/`loom`-style where the small deterministic state
   machine fits. This is the piece to *not* rush; any correctness trade-off pauses
   for owner review (same posture as the Raft increment).

Each sub-increment is its own small PR with tests and docs; a release may be cut at
the increment-3 boundary.

## Consequences

- **+** True elastic scaling: grow or shrink the cluster under live load with no
  downtime and minimal (`~1/N`) data movement — the property ADR-0065 was designed
  around, now realised.
- **+** Reuses what already exists: the router's `ArcSwap`, HRW's minimal reshuffle,
  and ADR-0030's replication stream for the slice copy — composition, not new
  machinery.
- **+** The coordinator stays off the data path, so a query never depends on it and
  a stale map self-heals via the redirect.
- **−** A new component to run (the coordinator) — strictly opt-in, single-node for
  now, and not required by single-node or a static cluster.
- **−** Migration is the most intricate code in the cluster so far; mitigated by
  staging 3a/3b before 3c, by the single-version-flip ownership model, and by the
  redirect that tolerates router disagreement.
- **−** No coordinator HA and no automatic *write* failover here — failover is
  increment 4 (audited Raft), and coordinator HA can later ride that same Raft.
  Stated honestly rather than implied.

## Alternatives considered

- **Consistent-hash ring with virtual nodes** — rejected: HRW already gives the same
  minimal-reshuffle property with no ring/vnode bookkeeping (ADR-0065). Stable ids +
  HRW are simpler.
- **Stop-the-world rebalance** (drain → copy → resume) — rejected: it is downtime,
  which fights the explicit "no downtime" goal; the online handshake exists precisely
  to avoid it.
- **Gossip / no central map** (each router merges peer views) — rejected for now: a
  single authoritative *versioned* map is far easier to reason about for the
  no-lost-writes flip; gossip is a later scale concern, not a correctness aid.
- **Routers read the map directly from the shards** (no coordinator) — rejected: no
  single authority for the version and the in-flight migration state, so a clean,
  atomic ownership flip is hard to guarantee.
- **Positional shard index as the HRW key** (the increment-1 form) — rejected for
  dynamic membership: removing a shard would re-key survivors and reshuffle
  everything; a stable id is required.
