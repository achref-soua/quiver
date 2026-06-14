# ADR-0026: HNSW incremental delete (soft-delete)

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Achref Soua

## Context

HNSW (ADR-0007) supported incremental insert but not delete: `Database::delete`
on an HNSW collection marked the index `stale`, so the next search rebuilt the
whole graph from the store — `O(N)` per delete. Under a churned workload that is
exactly the cost ADR-0023 removed for IVF. HNSW has no cheap in-place node
removal: true deletion means repairing the removed node's neighbors across every
layer, which is intricate and error-prone. A lighter mechanism is needed.

## Decision

**Soft-delete.** `Hnsw::mark_deleted(node)` records a node in an in-memory
`deleted` set in `O(1)`. The node stays in the graph as a connectivity waypoint —
its edges are untouched — but is filtered from query results: `search` traverses
the graph as before (through deleted nodes) and drops tombstoned nodes from the
returned candidates. To hold recall, the base-layer `ef` is **widened by the live
fraction** (`ef · total / live`, capped at the node count) so that roughly `ef`
*live* candidates survive the filter. `len()` reports the live count.

**Rebuild only when tombstones dominate.** `Database::delete` soft-deletes a live
HNSW in `O(1)`; once the deleted fraction reaches `HNSW_REBUILD_DELETED_FRACTION`
(0.2) it marks the handle `stale`, so the next access rebuilds from the store's
live rows (reusing the existing rebuild path) and reclaims the graph space. A
re-`upsert` of a soft-deleted id likewise rebuilds, since HNSW cannot update a
node in place.

**Scope: HNSW.** IVF removes in place (ADR-0023); the disk graph and Vamana stay
batch-built. Eager edge repair (re-linking a deleted node's neighbors) is
deliberately not done — the rebuild trigger bounds the degradation more simply.

## Crash-safety

HNSW is in-memory and **derived**: it is rebuilt from the store on open (ADR-0023's
stance). The durable record of a delete is the store's tombstone (ADR-0021),
`fsync`'d before the call returns; the index soft-delete is reconstructed (as an
absence) when the index is rebuilt. The index therefore never joins the
durability path, and the `kill -9` crash gate (R3, ADR-0005) is untouched —
consistent with ADR-0023 and distinct from the durable-index work of ADR-0025.

## Consequences

- **+** A delete is `O(1)` (a set insert) instead of an `O(N)` graph rebuild;
  rebuilds amortize to roughly once per 20% churn.
- **+** Recall is preserved while tombstones are present via the `ef` widening, and
  no deleted id is ever returned.
- **−** Tombstoned nodes occupy graph space and are traversed until the next
  rebuild, so very high churn between rebuilds slightly raises search work
  (bounded by the rebuild threshold and the `ef` cap).
- **−** No eager edge repair; a node's neighbors are not re-linked around it until
  a rebuild — a future refinement if churn patterns warrant it.

## Alternatives considered

- **Eager deletion with neighbor repair** (true removal + re-linking across
  layers) — rejected for this increment: HNSW deletion-with-repair is intricate
  and error-prone, and the rebuild trigger already bounds degradation. Revisit if
  profiling shows the rebuilds dominate.
- **Post-hoc filtering without `ef` widening** — rejected: tombstones in the `ef`
  window evict live candidates, dropping recall as deletions accumulate.
- **Keep rebuilding on every delete (status quo)** — rejected: `O(N)` per delete
  defeats incremental maintenance under churn — the same problem ADR-0023 solved
  for IVF.
