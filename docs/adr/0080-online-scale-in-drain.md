# ADR-0080: Automated online scale-in (reverse-migration drain)

- **Status:** Accepted
- **Date:** 2026-07-25
- **Deciders:** Achref Soua
- **Builds on:** [ADR-0066](0066-dynamic-cluster-membership.md) (dynamic membership,
  online slice migration), [ADR-0065](0065-cluster-mode-implementation.md) (cluster
  mode, autoscaling)

## Context

Growing a Quiver cluster is one call: `POST /cluster/shards/grow` adds the shard as
**joining**, and the coordinator copies its slice off the donors, flips ownership,
and drops the donors' copies — online, with the slice queryable throughout and no
acknowledged write lost ([ADR-0066](0066-dynamic-cluster-membership.md) increment
3c-ii).

Shrinking had no equivalent. `DELETE /cluster/shards/{id}` removes a shard from the
map immediately, which is correct only for a shard that is **already drained or
empty**. Removing a shard that still holds data loses that data: HRW instantly
reassigns its slice to the survivors, which have never seen those points. So the
documented shrink procedure was a manual, multi-step, operator-driven affair, and
the coordinator's autoscale policy could grow but never shrink. It is the concrete
gap the field guide names ("automating safe scale-*in* (a reverse-migration drain)").

## Decision

Add a **leaving** shard state — the mirror image of **joining** — and one endpoint,
`POST /cluster/shards/{id}/drain`, that runs the whole reverse migration in the
background.

### The symmetry

| | Grow (join) | Shrink (drain) |
| --- | --- | --- |
| Transitional state | destination is in the map **early**, marked `joining` | donor stays in the map **late**, marked `leaving` |
| Who owns the slice | the donor, until `promote` | the survivors, from `mark_leaving` |
| Who holds the data | the donor, until the copy completes | the leaving shard, until the copy completes |
| Dual-write target (`donor_for`) | the donor | the leaving shard |
| Searchable | donor only (joining excluded) | **both** (leaving shard included) |
| The flip | `promote(id)` | `remove_shard(id)` |
| Then | drop the donors' copies | drop the drained shard's copies |

Concretely, in `ShardMap`:

- **`mark_leaving(id)`** adds the id to a new `leaving: Vec<u64>` and bumps the
  version. `shard_for` **skips leaving shards**, so in that single atomic version
  ownership of the slice moves to the survivors — and it moves to *exactly* the
  owners the post-removal map will name, so the drain copies each point straight to
  its final home rather than to a staging shard.
- **`donor_for(id)`** gains the drain case: when the id's *pre-drain* owner (HRW over
  every shard) is a leaving shard, that shard is the donor. The router already
  dual-writes to `donor_for` and serves gets from it, so **the entire router data
  plane is unchanged** — no new routing code for the new direction.
- **`active_shards()`** (the search fan-out) keeps including leaving shards: they
  still hold points that have not been copied yet, and excluding them would hide
  those points. Where a leaving shard and a new owner both hold a copy, the router's
  existing id-dedup in the gather collapses them — the same dedup that already covers
  the post-`promote` window.
- **`cancel_leave(id)`** aborts, returning the shard to ownership of its slice.

The coordinator's `run_drain` then mirrors `run_migration` exactly: grace (routers
adopt the leaving map, dual-write live) → copy every point off the leaving shard to
its new owner → `remove_shard` (the flip) → grace → drop the copies it no longer owns.

### Invariants (the same two the grow path holds)

- **No acknowledged write is lost.** From `mark_leaving` onward every write to the
  slice lands on both the new owner (as the HRW owner) and the leaving shard (as the
  donor), so a write concurrent with the copy survives the removal regardless of
  which side of the copy cursor it falls on. An **abort** is equally safe: the
  leaving shard has been dual-written throughout, so it is current the moment its
  flag clears.
- **The slice stays queryable.** The leaving shard serves searches and gets for its
  slice until the removal, by which point the survivors hold it.

### A drop never removes the last copy

The copy is **get-if-absent**, and the drop is now **presence-checked**: a copy is
deleted from the source only after confirming the destination holds that point.
Anything the copy somehow missed is kept and logged rather than deleted on the
strength of an assumption. This applies to the grow path too — `copy_points` and
`drop_copies` are now one pair of helpers parameterized by "which URL should end up
holding this id", which is the only thing that differs between the two directions.

### What this deliberately does not do

- **No automatic policy-driven scale-in.** The drain is a safe *operation*; deciding
  to shrink on a low-water signal is a separate, opt-in decision. Shrinking on load
  invites flapping (drain out, refill, drain again), and unlike a grow it
  decommissions a machine. `AutoscaleConfig` stays scale-out only.
  *(ponytail: the operation is the hard part and it is now automated; the policy is
  three fields whenever a workload actually asks for it.)*
- **No new API for the router.** No shard-side changes at all — a drain is composed
  from the existing point APIs and the existing migration-aware routing.
- **Multivector collections abort the drain**, honestly, exactly as they abort a
  grow (the paginated scroll path does not carry token matrices). Failing loudly
  beats silently dropping a slice.
- **One migration at a time.** A drain is refused while a join is in flight, and
  `mark_leaving` refuses to drain the last remaining destination.

## Consequences

- **+** Scale-in is now one authenticated call, with the same safety properties as
  scale-out, and it is single-box testable — the new test drains a shard out of a
  live 3-shard cluster under concurrent writes and asserts both invariants.
- **+** The router's data plane did not change. The drain rides `donor_for` /
  `shard_for` / the gather dedup, all of which already existed for the grow path;
  the new state is ~60 lines in the map plus the coordinator's mirrored loop.
- **+** The destructive step is now verified on both paths (presence-checked drop),
  which is a strict improvement to the pre-existing grow path.
- **−** The drain's copy is `O(points on the shard)` HTTP round-trips with a presence
  check each — fine for the cluster sizes this coordinator targets, and identical in
  shape to the grow copy, but not a bulk-stream. The streaming replication path
  (ADR-0030) is the upgrade if a drain ever needs to move a very large shard.
- **−** During a drain the slice is written twice and searched on two shards, so the
  cluster does modestly more work until it finishes — bounded by the copy, and the
  price of staying online.
- **−** `leaving` is a second transitional flag on the map. It is serialized with
  `#[serde(default)]`, so a coordinator state file written by an older build loads
  unchanged, and `remove_shard` clears both flags so no stale id can skew routing.

## Alternatives considered

- **Reuse `joining` for both directions** (mark the *survivors* as joining and let
  the leaving shard be an ordinary donor). Rejected: the joining flag also excludes a
  shard from search, so the survivors — which serve the rest of the cluster's data —
  would drop out of every query for the duration of the drain. The two states are
  genuinely different in the read path; a distinct flag is honest.
- **Copy first, remap after** (leave the map alone during the copy, then remove the
  shard in one step). Rejected: it leaves a window where the copied points are owned
  by the shard that is about to disappear, so a write during the copy lands only on
  the departing shard and is lost at the removal. Moving ownership *first* and
  dual-writing back is what makes the handoff lossless.
- **Drain by replicating the whole shard** (point the survivors at it as followers,
  ADR-0030). Rejected: replication copies a shard wholesale, but a drain must
  *scatter* one shard's points across N survivors by HRW ownership — a different
  operation, and it would replicate points the survivors must not own.
- **Leave the drained shard's data in place** instead of dropping it. Tempting as a
  safety copy, but a decommissioned node that is later restarted and re-added would
  serve stale points — including ones deleted in the meantime — under a new id. The
  drained shard is emptied, and every deletion is presence-checked first.
- **Do nothing (keep the manual drained delete).** Rejected: it is the last named
  gap in elastic scaling, it is entirely self-doable and single-box testable, and a
  manual multi-step data-movement procedure is exactly where operators lose writes.
