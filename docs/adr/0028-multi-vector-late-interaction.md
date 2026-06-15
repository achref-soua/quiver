# ADR-0028: Multi-vector documents & late-interaction (ColBERT) retrieval

- **Status:** Proposed
- **Date:** 2026-06-15
- **Deciders:** Achref Soua

## Context

Single-vector ("dense") retrieval encodes a whole document into one embedding and
ranks by a single distance. It is fast and compact, but it loses token-level
detail: a query term that matters can be averaged away in a document's pooled
vector. **Late-interaction** models — ColBERT (Khattab & Zaharia, SIGIR'20) and
its successors ColBERTv2 / PLAID — instead represent a document as a *set* of
token embeddings and score a query (also a set of token embeddings) by **MaxSim**:

```
score(Q, D) = Σ_{q ∈ Q}  max_{d ∈ D}  sim(q, d)
```

Each query token is matched to its most similar document token, and the per-token
maxima are summed. This recovers fine-grained matching and is consistently
stronger than single-vector retrieval out of domain (e.g. BEIR), at the cost of
storing many vectors per document — typically 100–200 token vectors at dim 128.

That storage cost is exactly the axis Quiver is built for. Quiver's wedge is
**memory frugality** — disk-resident DiskANN/Vamana, IVF, and PQ/scalar/binary
quantization with exact re-rank (ADR-0007/0008/0019). A ColBERT corpus is a large
pool of low-dimensional vectors: precisely the workload PQ and the disk-resident
index were designed to compress. Supporting late interaction turns ColBERT's
headline weakness into a demonstration of Quiver's headline strength, and it is
an open differentiator that the incumbents gate or omit.

The hard constraints are unchanged: this must not regress durability (ADR-0005)
or the security posture (encryption-at-rest, ADR-0010), and it should reuse the
storage and index machinery already built (ADR-0020 row segments, ADR-0023
incremental IVF, ADR-0022 secondary-index filters) rather than fork a parallel
engine.

## Decision

**Model a multi-vector document as a group of ordinary rows — one row per token
vector — over the existing row-addressed store, and add a late-interaction
(MaxSim) scoring layer on top. No on-disk format changes.**

A collection created `multivector` stores, for each document, its token vectors as
a contiguous run of rows in the same `.vec`/`.pay`/`.dir` segments that
single-vector collections use (ADR-0020). The token pool *is* the set indexed by
the collection's ANN index (HNSW / IVF / DiskANN, per its `IndexSpec`), so
candidate generation is a standard nearest-neighbour search over token vectors —
the PLAID retrieval shape — and it inherits encryption-at-rest, tombstones /
compaction, and the incremental IVF path for free.

**1. Token-as-row storage (no format change).** Each token vector occupies one
row whose external id encodes its parent document and its ordinal
(`<doc-id><US><ordinal>`, where `<US>` is the ASCII Unit Separator `0x1F`,
disallowed in user document ids). The document's payload is stored once, on its
first ("anchor") token row; the other token rows carry an empty payload. Because a
token vector is an ordinary fixed-stride row, **nothing new joins the fsync /
crash path** — the `kill -9` gate (ADR-0005) is untouched by construction, the
same de-risking ADR-0023 relied on.

**2. Document grouping is derived, in-memory.** The embeddable database maintains,
per multi-vector collection, a `doc-id → token rows` map (and the reverse), built
on open by parsing row ids — exactly how `int_to_ext` / `ext_to_int` are already
derived from the store on open. No grouping state is persisted; the store remains
the single source of truth.

**3. MaxSim scoring is a pure, shared function.** `quiver-index` gains
`max_sim(query_tokens, doc_tokens, metric)` beside the existing
`ordering_distance` / `report_metric`, reusing the `quiver-simd` kernels so an
index search and a MaxSim re-rank never use divergent math. Late interaction is
defined for similarity metrics: a `multivector` collection requires **Cosine or
Dot** (MaxSim sums per-token maxima of a *similarity*); **L2 is rejected** at
creation, as Dot already is for some index kinds.

**4. Two-stage retrieval (PLAID shape) in the embeddable database.**

- *Candidate generation* — for each of the query's token vectors, search the
  token-pool index for its `k'` nearest token rows; map each hit to its parent
  document; the (bounded) union is the candidate document set.
- *Re-ranking* — for each candidate document, gather its token vectors (resident
  in the index for flat storage, read from the encrypted store for PQ / disk),
  compute the full MaxSim against all query tokens, apply the document-level
  payload `Filter` (evaluated on the anchor payload), and return the top-`k`
  documents by MaxSim.

This re-uses the hybrid-search discipline: filters stay exact (re-checked on
survivors), and the frugal index kinds keep PQ codes in RAM with exact vectors on
disk for the re-rank.

**5. Additive wire & SDK surface; the single-vector path is untouched.** New,
separate messages / RPCs (`Vector`, `MultiVectorPoint`, `UpsertMultiVector`,
`SearchMultiVector`) carry token sets and return document-level matches; the
existing `Upsert` / `Search` and their DTOs are unchanged. The Python / TypeScript
SDKs and the MCP server gain matching multi-vector upsert / search entry points. A
`multivector` collection rejects single-vector `Upsert` / `Search` (and
vice-versa) with a clear error.

**6. Scope.** This ADR delivers in-memory-derived document grouping + MaxSim over
the existing durable store, with any index kind backing the token pool. It does
**not** add a bespoke on-disk multi-vector layout, residual compression
(ColBERTv2), or centroid-interaction pruning (PLAID's full optimization) — those
are later increments with their own ADRs if measurement shows they are needed.
Token-level PQ — the frugality story — is the existing IVF+PQ / disk path applied
to the token pool, not new code.

## Consequences

- **+** True late-interaction quality (token-level MaxSim, PLAID-style candidate
  generation) with **zero on-disk format change** and the crash gate untouched —
  the feature rides the existing durable, encrypted row store and ANN indexes.
- **+** The memory-frugality wedge is showcased, not strained: a ColBERT token
  pool compresses under the existing IVF+PQ / disk-resident path (e.g. 128-dim
  f32 → PQ codes ≈ 32× smaller in RAM, exact vectors on encrypted SSD for the
  re-rank).
- **+** It composes with everything already built — encryption-at-rest, tombstones
  / compaction, incremental IVF, secondary-index filters — because a token vector
  is just a row.
- **−** A document write / delete fans out to `N` row operations, and the id space
  inflates ~`N`× (`N` = tokens/doc); bounded, and precisely the regime PQ / disk
  address. The grouping maps cost `O(total tokens)` RAM, like the existing id
  maps.
- **−** The re-rank reads each candidate document's token vectors; for PQ / disk
  collections that is `O(candidates × tokens/doc)` decrypt-on-demand reads (the
  standard PLAID re-rank cost), bounded by the candidate cap.
- **−** Reserving `0x1F` in document ids and storing the payload on the anchor row
  are conventions the embed layer must enforce and document.
- **−** A second, parallel API surface (multi-vector upsert / search) to maintain
  across proto / server / SDKs / MCP — kept strictly additive so the single-vector
  path carries no cost.

## Verification (plan)

- **`quiver-index`** — unit tests for `max_sim`: the MaxSim identity (sum of
  per-query-token maxima), peaks on aligned tokens, Cosine vs Dot behaviour, and
  degenerate (empty query / empty document) cases; a differential check against a
  brute-force reference.
- **`quiver-embed`** — end-to-end: upsert documents as token sets →
  `search_multi_vector` returns documents ranked by MaxSim, equal to a brute-force
  MaxSim over the whole corpus (exactness on a small set); the document-level
  filter is honoured and exact; **reopen** rebuilds the doc↔token grouping and
  returns identical rankings; a per-document delete removes all its tokens; and the
  **`kill -9` crash gate is re-run and stays green** (multi-vector adds ordinary
  rows only — asserted, not assumed).
- **Wire / SDK** — REST + gRPC round-trip a multi-vector upsert + search; a
  `multivector` collection rejects single-vector ops and vice-versa; the
  Python / TS SDK and MCP paths are covered by their suites.
- **Frugality** — a documented example serves a token pool from PQ codes with
  exact re-rank (recall preserved, RAM reduced), host-independent figures only;
  raw RSS stays reference-hardware-gated and is never fabricated.
- Coverage stays ≥ 80% on the core-engine crates; `just verify` green on every PR.

## Alternatives considered

- **Native multi-vector row (variable-stride `.vec`)** — one document = one row
  whose vector slot holds a `[tokens × dim]` matrix. Cleaner document-level API
  (one id per doc, one payload), but it changes the on-disk segment format to
  variable stride and so **joins the crash path** — the high-risk surface
  ADR-0020/0023 deliberately kept fixed. It *also* still needs a token-pool index
  for scalable candidate generation, so it adds risk without removing work.
  Rejected for the first increment; revisitable if token-as-row's id inflation
  proves limiting.
- **Pooled coarse vector + token matrix in a side column, re-rank only** — index a
  single pooled vector per document for stage 1, store the token matrix only for
  the MaxSim re-rank. Lower implementation cost, but stage 1 becomes a
  single-vector approximation that misses the token-level matches late interaction
  exists to capture — it caps recall below real ColBERT. Rejected: it would ship
  the storage cost of multi-vector without its retrieval quality.
- **A dedicated multi-vector storage engine / format** (per-document vector blocks,
  residual compression à la ColBERTv2, centroid pruning à la PLAID) — the
  state-of-the-art for billion-scale ColBERT, but a large parallel storage + index
  subsystem off the existing crash / encryption path. Rejected for now as
  premature: token-as-row reaches production quality by reusing the engine, and
  these optimizations can be layered later behind their own ADRs if measurements
  demand them.
- **Replicating the document payload onto every token row** (instead of an anchor
  row) — simpler filtering (any token row carries the payload) but multiplies
  payload storage and the secondary index by `N`. Rejected: anchor-row storage
  keeps payload / `.sec` costs document-shaped.

## References

- O. Khattab, M. Zaharia. *ColBERT: Efficient and Effective Passage Search via
  Contextualized Late Interaction over BERT.* SIGIR 2020.
- K. Santhanam et al. *ColBERTv2: Effective and Efficient Retrieval via
  Lightweight Late Interaction.* NAACL 2022.
- K. Santhanam et al. *PLAID: An Efficient Engine for Late Interaction Retrieval.*
  CIKM 2022.
