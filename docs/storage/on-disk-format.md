# Storage Engine & On-Disk Format

The storage engine (`quiver-core`) owns all durable state. It is built from scratch — no embedded KV/DB engine. This document specifies the on-disk layout; durability/recovery is detailed in [ADR-0005](../adr/0005-durability-and-recovery.md), the byte-level format choices in [ADR-0004](../adr/0004-on-disk-format.md), and serialization in [ADR-0003](../adr/0003-serialization.md).

## Goals

- **Durable:** an acknowledged write survives `kill -9` and power loss.
- **Crash-consistent:** no torn or partially-visible state after recovery; corruption is *detected*, never silently served.
- **Encrypted-at-rest by default:** ciphertext on disk with secure defaults, including index artifacts.
- **Memory-frugal:** vectors are read through `mmap`; only the working set is resident.
- **Snapshot-friendly:** immutable segments + a versioned manifest make consistent backups cheap.

## Directory layout

One data directory per Quiver instance:

```text
<data_dir>/
├─ CURRENT                     # one line: name of the live manifest file (atomic pointer)
├─ manifest-000123             # versioned manifest snapshots (append-only set)
├─ wal/
│  ├─ wal-000045.log           # write-ahead log segments (rotated)
│  └─ wal-000046.log
└─ collections/
   └─ <collection_id>/
      ├─ descriptor            # encrypted collection schema (dim, dtype, metric, fields)
      ├─ segments/
      │  ├─ seg-000010.vec      # vector column (fixed stride)
      │  ├─ seg-000010.pay      # payload heap (variable length)
      │  ├─ seg-000010.sec      # secondary indexes (value → row bitmap)
      │  └─ seg-000010.del      # tombstone bitmap (roaring)
      └─ index/
         ├─ hnsw-000010.graph   # index artifact(s) (HNSW graph / Vamana / IVF lists)
         └─ pq-000010.codebook  # quantizer codebooks
```

`<collection_id>` and segment numbers are monotonic; file names are immutable once written.

## Pages: the encryption, checksum, and I/O unit

All paged files (`.pay`, `.sec`, manifest, index artifacts) are a sequence of fixed **16 KiB pages**. A page — not a record — is the unit of checksumming, encryption, and buffer management. (The `.vec` column is *stride-addressed* rather than paged for record access, but is still encrypted/checksummed in 16 KiB blocks; see below.)

**Plaintext page layout** (before at-rest encryption):

```text
┌──────────────── 32-byte header ─────────────────┐
│ magic:u32 │ format_ver:u16 │ page_type:u8 │ _pad │
│ page_id:u64                                      │
│ lsn:u64           (last LSN that modified page)  │
│ payload_len:u32   │ crc32c:u32 (of header+body)  │
├──────────────────── body ───────────────────────┤
│ ... up to (16384-32) bytes of page payload ...   │
└──────────────────────────────────────────────────┘
```

**On disk, when encryption-at-rest is enabled**, each 16 KiB plaintext page is sealed with an AEAD into a same-aligned on-disk block:

```text
[ nonce:12 ][ ciphertext(plaintext_page) ][ tag:16 ]
```

The AEAD tag authenticates the ciphertext (tamper/corruption detection); the inner CRC32C additionally protects the plaintext path and the encryption-disabled mode. Nonces are unique per (key, page-version) by construction — see [`../security/crypto.md`](../security/crypto.md). All integers are **little-endian**; the header is 8-byte aligned.

## Vector column (`.vec`)

A dense, fixed-stride column: `stride = dim × sizeof(dtype)`, `dtype ∈ {f32, f16, bf16, int8, binary}`. Record at row `r` lives at byte offset `r × stride` (within the segment's logical vector region). This gives O(1) random access and cache-friendly scans for exact re-ranking. The file is encrypted/checksummed in 16 KiB-aligned blocks so a single vector may straddle block boundaries without per-record overhead. Vectors are **immutable**; an update writes a new row in the active segment and tombstones the old one.

## Payload heap (`.pay`) and the row directory

Variable-length payloads (validated JSON, see ADR-0003) live in a paged heap (`seg-*.pay`). A per-segment **row directory** (`seg-*.dir`) maps `row → (external_id, payload offset, payload len)`, serialized as a paged `postcard` blob so it inherits per-page CRC integrity. Filterable fields declared in the collection schema are additionally extracted into secondary indexes at flush time. The external id (caller-supplied string / `u128`) maps to `(segment, row)` via a **primary index**; identity resolution and updates go through this map. The current implementation rebuilds the primary index on open from the segment directories (oldest-to-newest) plus the WAL tail (ADR-0020); checkpointing it to disk with the manifest is a deferred open-latency optimization.

## Secondary indexes (`.sec`)

Per filterable field (declared in the collection schema), value → **roaring bitmap** of the rows that hold it, stored as sorted `(key → bitmap)` entries with **order-preserving keys** — UTF-8 for keyword fields, a sign-flipped big-endian encoding for numeric fields (so negatives sort correctly):

- **equality / `in`**: binary-search the key(s);
- **range** (`<`, `>`, `between`): a contiguous scan of the sorted keys;
- (geo and full-text are later phases).

Indexes are built per filterable field **at flush** (parsing each row's JSON payload) and rebuilt at compaction; a collection with no filterable fields writes no `.sec`. They are immutable like `.vec`/`.pay`/`.dir` — deletes and updates are reflected through the `.del` bitmap and the primary index, never by rewriting `.sec`. At query time the per-segment results are unioned and a hit is kept only if the primary index still points at that `(segment, row)`, so each id is counted once with its live value; un-checkpointed (active-buffer) rows are evaluated directly. The planner uses selectivity to choose pre- vs post-filtering (see [`../index/design.md`](../index/design.md)). Full decisions: [ADR-0022](../adr/0022-secondary-indexes.md).

## Write path, sealing, and compaction

1. **Active segment (in memory).** Upserts append to an in-RAM active segment, *after* the WAL record is durably appended. The vector goes into the live index immediately.
2. **Seal & flush.** When the active segment exceeds a size/row threshold it is **sealed** (made immutable) and flushed to `seg-NNNNNN.*` files: write data → `fsync` files → `fsync` directory → publish a new manifest version that references the segment → `fsync` manifest → atomically swap `CURRENT`. A crash *before* the swap leaves the segment orphaned (GC'd on recovery); *after*, it is durable. There is never a partially-visible segment.
3. **Tombstones & deletes.** Segments are immutable; a delete or an update is first `fsync`'d to the WAL, then — at the next checkpoint — the dead `(segment, row)` is merged into that segment's `.del` roaring bitmap, which is rewritten atomically (temp + rename). A row tombstoned in its segment is skipped on recovery. The crash-safety argument (atomic `.del` writes, monotonic deletes, WAL backstop) is [ADR-0021](../adr/0021-tombstones-and-compaction.md).
4. **Compaction.** A background job merges small segments and rewrites live (non-tombstoned) rows into a fresh segment, then atomically updates the manifest and reclaims the old segments and their index artifacts. Compaction is crash-safe for the same reason flush is: old inputs remain valid until the manifest swap.

## Manifest: catalog + durability anchor

The manifest is the source of truth for *what is live*. Each version records, per collection: the live segment set with their LSN ranges; index checkpoint pointers; the schema; and the **last checkpointed LSN** (the WAL position safely captured in segments). Manifests are written as new immutable files and made current via the **write-new + fsync + atomic-rename of `CURRENT`** protocol (LevelDB-style). This makes catalog updates atomic and gives us free point-in-time snapshots.

## Snapshots & backup

Because segments are immutable and manifests are versioned, a **snapshot** pins a manifest version plus the WAL tail beyond its checkpoint LSN; the referenced segment files are copied or hardlinked. **Restore** points `CURRENT` at the snapshot manifest. Backups are naturally **incremental** — only segments created since the last backup need shipping (e.g., to S3/MinIO).

## Format versioning & magic

Every file begins with a magic number and a `format_ver`. The reader refuses unknown majors and migrates known older minors. Magic constants per file kind (`.vec`/`.pay`/`.sec`/manifest/WAL) prevent cross-type confusion. The wire/storage format version is independent of the product SemVer and is bumped only on layout changes, with a migration note.

## What recovery does (summary)

On open: read `CURRENT` → load the manifest → for each WAL record with `LSN > last_checkpointed_LSN`, replay it idempotently into a fresh active segment and the index; discard a torn trailing WAL record (its framing CRC fails — it was never acknowledged); GC orphaned segment files not referenced by the manifest. Full algorithm and the durability guarantee: [ADR-0005](../adr/0005-durability-and-recovery.md). The crash-recovery test harness that gates `v0.1.0` is described in [`../risk-register.md`](../risk-register.md) (R3).
