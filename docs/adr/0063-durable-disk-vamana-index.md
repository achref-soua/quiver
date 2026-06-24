# ADR-0063: Durable on-disk DiskVamana index (load-on-open)

- **Status:** Accepted — implemented in `v0.23.0`.
- **Date:** 2026-06-24
- **Deciders:** Achref Soua
- **Relates to:** [ADR-0019](0019-disk-resident-vamana.md) (the disk-resident
  graph), [ADR-0025](0025-durable-incremental-index.md) (durable IVF — the
  template this mirrors), [ADR-0033](0033-graph-incremental-freshdiskann.md)
  (FreshDiskANN base+delta), [ADR-0007](0007-memory-frugality.md) (the wedge).

## Context

The disk-resident DiskVamana index is Quiver's memory-frugality wedge: the
graph and full-precision vectors live in an encrypted, `mmap`-ed on-disk file;
only the PQ codes (and a small FreshDiskANN delta) stay resident; an exact
re-rank reads vectors from the map (ADR-0019). Serving a 10 M × 768-d collection
should cost ~1 GB of RAM, not the ~31 GB the full-precision vectors would.

That wedge was **never realised on a running server**, because the index was
still *derived on open*. `load_index` had a fast-path only for IVF (ADR-0025);
every other kind — including DiskVamana — fell through to `rebuild_index`, which
`scan_collection`s **every full-precision vector into RAM** (~3.8 GB for GIST1M)
and re-runs `Vamana::build` + PQ training on each cold open. The frugal
`vamana.qvx` artifact was written and then **ignored** at startup. So after any
restart the server paid the full-RAM, `O(N)` rebuild and served from that
rebuild's allocator high-water mark — which is exactly why the GIST1M benchmark
RSS was the worst in the field, and why the disk-path serving RSS had to be
*omitted* as untrustworthy (the v0.22.0 report's "post-build allocator
high-water mark, not the cold-reload serving footprint" caveat).

This ADR makes the DiskVamana index **durable**: a restart *loads* the mmap'd
base instead of *rebuilding* it, so the server finally serves from the frugal
PQ-codes-resident path — the wedge becomes real and measurable, plus a large
startup-time win. The hard constraint is unchanged from ADR-0005/0025: a process
kill at any point recovers with no lost acknowledged writes and no corrupted
state; the index stays a fast path over the authoritative store.

## Decision

**Mirror ADR-0025, adapted to a file-backed base.** The IVF snapshot serialises
its whole in-RAM state to a blob. DiskVamana is different: its bulk (graph +
full vectors) already lives in an immutable on-disk file, and only the PQ codes
and the FreshDiskANN delta are in RAM. So we split durability in two:

**1. The base file is the durable artifact, published atomically.** The
batch-built graph + PQ are sealed to `vamana.qvx` (encrypted with the
collection's page codec, ADR-0010/0019) by a **write-temp → fsync →
rename → fsync-dir** sequence, so a referenced base file is always complete —
a crash mid-write leaves the previous complete file or none, never a torn one
(the same atomic-publish discipline ADR-0025 uses for the snapshot blob). The
base is written only at **build / consolidation** (ADR-0033 StreamingMerge), so
it keeps its write-once contract between consolidations.

**2. A tiny checkpoint blob ties the base to the live state.** At each
checkpoint, a non-stale DiskVamana seals a small blob via the existing
`checkpoint_with_index_snapshots` path (ADR-0025), referenced from the manifest
at the checkpoint LSN. The blob carries only:

- `base_row_count` — the base graph's point count, to validate the `mmap`'d file
  against the blob on open (a mismatch ⇒ rebuild);
- `deleted_ids` — the FreshDiskANN tombstone set;
- `int_to_ext` — the id map (as IVF's envelope does).

The **delta vectors are not stored** — they already live in the store's sealed
segments. Delta ids are *implied* as `[base_row_count, int_to_ext.len())` (insert
order assigns delta points the internal ids above the base), so on open each is
re-fetched by id via `Store::get` and re-inserted into the in-memory delta. The
blob is therefore O(delta + tombstones) ids, never O(N) vectors — it cannot
itself reintroduce a full-RAM footprint.

**3. The data WAL is the index's log (no second log).** Between checkpoints the
index is not separately journaled; the `Upsert`/`Delete` WAL records ADR-0005
already `fsync`s before acknowledging are replayed on open into the delta (the
post-checkpoint `recovery_tail`), exactly as for IVF.

**4. Recovery (extends ADR-0025).** On open, for a DiskVamana collection: if the
manifest references a blob, decode it, `mmap` `vamana.qvx`, **validate
`base.len() == base_row_count`**, reconstruct the delta from the store by the
implied ids, apply the tombstones, then replay the post-checkpoint WAL tail.
**Any problem at any step — absent/torn/mismatched base, decode failure, a
missing delta row — falls back to the authoritative full rebuild.** The frugal
load is always optional; correctness rests on the store, never on the artifact.

**5. Crash-safety rests on the same three facts as ADR-0025**, not on new
cross-file atomicity: atomic publish (base via rename, blob via the manifest
swap), immutability (a published base/blob is read-only until the next
build/checkpoint writes a new one), and the WAL backstop (post-snapshot
mutations are `fsync`'d before ack and replayed). A base a crash left behind the
WAL is caught up by replay; a base a crash tore or a blob it lost falls back to
rebuild.

**6. Scope: the single-vector DiskVamana kind.** HNSW/Vamana (in-memory) keep
deriving on open; multivector/ColBERT rebuild on open by design. No on-disk
*format* change: `vamana.qvx` keeps its ADR-0019 layout — it is now written
atomically and *read* on open rather than ignored. A store without a disk blob
(pre-0063, or a torn one) always recovers via the existing rebuild, so the
change is backward-compatible and opt-in per index kind. Default single-node
behaviour and the in-memory path are untouched.

## Consequences

- **+** A restart **loads** the mmap'd base (`O(1)` map, PQ codes resident) and
  reconstructs only the bounded delta, instead of an `O(N)` full-RAM rebuild —
  the memory-frugality wedge is finally real on a running server, and cold open
  is dramatically faster and lighter.
- **+** No second WAL, no new record types, no new manifest artifact *kind*: the
  blob rides the existing IVF snapshot path; the base file rides the existing
  index dir with an atomic rename.
- **−** The base file becomes first-class durable state: a checkpoint of a
  DiskVamana collection now also seals a (tiny) blob, and the crash gate's
  surface grows to the base file (covered by atomic rename + the rebuild
  fallback).
- **−** On open the delta is reconstructed by `Store::get` per delta id (bounded
  by the consolidation threshold), a small cost paid once per restart.
- **−** The derived **sparse** inverted index (hybrid search) is not persisted —
  as for the IVF durable path, a durably-loaded collection leaves it `None`, and
  hybrid search falls back to the correct-but-slower store scan until the next
  rebuild. Correctness is unaffected; only hybrid throughput is, on the first
  restart, for a collection that actually carries sparse payloads.
- **−** Still scoped to the single-vector disk graph; other kinds rebuild until
  their own increment.

## Verification

- **Restore + replay correctness (`quiver-embed`).** A reopened DiskVamana is
  *genuinely loaded* from the base (the preserved insertion-order id map diverges
  from a rebuild's reindex), top-k matches a pre-restart query and a fresh-rebuild
  ground truth, and the post-checkpoint WAL tail (upserts, deletes, in-place
  updates) replays correctly.
- **Fallback.** A removed/truncated base file, a `base_row_count` mismatch, and a
  decode failure each fall back to an authoritative rebuild that still answers
  correctly — proving the artifact is never load-bearing for correctness.
- **Artifact durability.** Two mechanisms, neither new: the blob rides the
  existing manifest-swap crash gate (the Store-level `crash_recovery` test already
  `SIGKILL`s across snapshot write / manifest swap / GC), and the base file is
  published by **atomic rename** (write temp → fsync → rename → fsync dir), so by
  construction a crash leaves a complete prior base or none — never a torn file a
  load could `mmap`. The fallback tests prove the remaining case directly: a base
  that is **absent or truncated** (the observable result of a torn write) is
  detected (`DiskVamana::open`'s per-page AEAD/length checks, or the
  `base_row_count` mismatch) and recovered by an authoritative rebuild, so "reopen
  never errors and never loses an acknowledged write" holds with the artifact as a
  pure fast path.
- **Frugality (dev-box · indicative; absolute headline reference-hardware-pending).**
  A reopened disk collection serves without ever allocating the full-precision
  vector set — asserted structurally (no `O(N)` rebuild path taken) and measured
  by the disk-path RSS lane in `bench/` and `scripts/bench-disk-frugality.ps1`.

## Alternatives considered

- **Store the delta vectors in the blob** — rejected: the delta can be ~20 % of
  the base, so the blob would reach hundreds of MB at GIST/Deep scale and
  reintroduce a full-RAM footprint on open. Re-fetching delta vectors from the
  store by id keeps the blob to id lists.
- **Re-seal (consolidate) the base at every checkpoint so the delta is always
  empty** — rejected: an `O(N)` graph write per checkpoint defeats incremental
  maintenance; consolidation stays churn-triggered (ADR-0033).
- **Versioned base files (`vamana-NNN.qvx`) + GC** — deferred as unnecessary:
  a single atomically-renamed `vamana.qvx` plus the `base_row_count` validation
  and rebuild fallback is crash-safe; the only cost of the single-name scheme is
  a rare extra rebuild if a crash lands in the narrow window between the base
  rename and the manifest swap. Revisit if that window ever matters.
- **Keep deriving on open (status quo)** — rejected: it is the bug; an `O(N)`
  full-RAM rebuild on every restart is exactly what made the disk path the worst
  RSS in the field.
