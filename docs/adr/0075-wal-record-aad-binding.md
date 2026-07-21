# ADR-0075: Bind the WAL record position into its AEAD AAD

- **Status:** Accepted
- **Date:** 2026-07-21
- **Deciders:** Achref Soua

## Context

Encryption-at-rest seals every WAL record with XChaCha20-Poly1305 via
`PageCodec::seal_record` ([ADR-0010](0010-key-management.md), `codec.rs`). Page
blocks already bind their **position** into the AEAD: `seal(page_id, …)` folds
`page_id` into both the derived subkey and the additional authenticated data
(AAD), so a sealed block cannot be silently relocated to a different page slot and
still authenticate. WAL records did **not**: `seal_record` passed an **empty AAD**,
so a record's ciphertext was authenticated only against itself.

The consequence (audit finding I3): an adversary with write access to the log
files — but not the key — could **reorder, duplicate, or relocate** whole sealed
records within the WAL and every one would still decrypt and authenticate. The CRC
protects against bit-rot, not a deliberate cut-and-paste of intact records, and
the record's own logical `lsn` lives *inside* the ciphertext, so it cannot police
the record's physical position. Replaying a reordered/duplicated log can corrupt
recovered state.

## Decision

Bind each record's **position** — its frame byte offset within the WAL file — into
the record AEAD's AAD, exactly as pages bind `page_id`. A record sealed at offset
*X* fails authentication if presented at any other offset, so reordering,
duplication, and relocation are detected on open.

The `PageCodec` record methods take an `Option<u64>` position:

```rust
fn seal_record(&self, pos: Option<u64>, plaintext: &[u8]) -> Result<Vec<u8>>;
fn open_record(&self, pos: Option<u64>, sealed: &[u8]) -> Result<Vec<u8>>;
```

- **`Some(offset)`** — the AEAD codec folds `offset.to_le_bytes()` into the AAD.
  The writer always seals with `Some(frame_offset)`; `WalWriter` tracks its write
  offset so no seek/`stat` is needed.
- **`None`** — an empty AAD, byte-identical to the pre-ADR behaviour. Used only to
  read **legacy (v1) WAL files** (see Compatibility). `PlainCodec` ignores the
  argument (the record methods are the identity transform when encryption is off).

`WAL_FORMAT_VERSION` bumps **1 → 2**. New WAL files are written as v2 and their
records are position-bound. `replay_wal` accepts v1 **and** v2: it opens a v2
file's records with `Some(offset)` and a v1 file's with `None`.

## Consequences

- **Tamper-evidence on the crash path.** Reordering, replaying, or relocating an
  intact sealed record within the log now fails AEAD authentication on recovery — a
  hard error, not a silently replayed frame. This closes I3.
- **No data-loss on upgrade.** The store always *creates* fresh WAL files (and
  rotates on checkpoint), so everything a v2 binary writes is position-bound. The
  only v1 files a v2 binary reads are un-checkpointed logs left by an unclean
  shutdown *before* the upgrade; those replay unchanged via the `None` path. A
  clean shutdown checkpoints and drains the WAL, so the common upgrade sees only
  v2 files.
- **Plaintext deployments are unaffected.** `PlainCodec`'s record methods are the
  identity transform; the position argument is inert. Only encrypted WALs change.
- **Scope of the binding.** The offset pins a record to its position *within a
  file*. A whole-file swap at identical offsets under the same key is out of scope
  here — the WAL sequence numbering and manifest/checkpoint state bound which files
  are authoritative, and per-file identity binding is a possible future tightening
  (fold `base_lsn` in as well).

## Alternatives considered

- **Bind the logical `lsn` instead of the byte offset.** Rejected: the `lsn` is
  serialized *inside* the encrypted record, so it is unavailable before decryption
  — the AAD must be a value the reader knows without the key. The frame offset is
  that value, and the reader already tracks it while scanning.
- **Bump the version gate and refuse v1 files.** Rejected: a v2 binary would then
  fail to recover an un-drained encrypted WAL from a pre-upgrade crash — data loss.
  The `None`/`Some` split reads v1 losslessly while writing only v2.
- **Fold the position into the subkey (like pages do) as well as the AAD.**
  Deferred: AAD binding alone gives the tamper-evidence; a per-offset subkey would
  defeat the cached-PRK derivation for no additional guarantee here.
