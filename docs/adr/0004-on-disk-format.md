# ADR-0004: On-disk format

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

The storage engine is built from scratch (ADR-0001). We must fix the physical layout: the unit of I/O/checksum/encryption, how vectors and payloads are stored, how integrity is checked, and how the format evolves. The full specification lives in [`../storage/on-disk-format.md`](../storage/on-disk-format.md); this ADR records the load-bearing choices.

## Decision

- **16 KiB pages** are the unit of checksum, encryption, and buffering for paged files; the `.vec` column is stride-addressed but still encrypted/checksummed in 16 KiB-aligned blocks.
- **Integrity:** every page carries a **CRC32C** over its plaintext; when encryption-at-rest is on, each page is additionally sealed with an **AEAD** whose tag authenticates the ciphertext. CRC32C is chosen for hardware acceleration (SSE4.2/ARM CRC) and good error detection.
- **Vectors** are stored in a **dense fixed-stride column** (`dim × sizeof(dtype)`) for O(1) access and cache-friendly exact re-ranking; vectors are immutable (update = new row + tombstone).
- **Endianness:** little-endian throughout; headers 8-byte aligned.
- **Catalog:** a **versioned manifest** made current via write-new + fsync + atomic-rename of `CURRENT` (LevelDB-style), enabling atomic catalog updates and cheap snapshots.
- **Versioning:** per-file magic + `format_ver`; unknown majors are refused, known older minors migrated. Storage format version is independent of product SemVer.

## Consequences

- **+** Random vector access is O(1); integrity is end-to-end (CRC + AEAD); catalog updates and snapshots are atomic; the format is explicitly versioned and migratable.
- **−** 16 KiB pages can waste space for tiny collections (acceptable; amortized by many vectors per page and by compaction). A fixed page size is a deliberate simplification for v1.

## Alternatives considered

- **4 KiB pages** — less internal fragmentation but cannot hold a single high-dim vector and raises per-page AEAD overhead. Rejected for the vector workload.
- **Record-per-page** — wasteful and awkward for dense vector columns.
- **xxHash/CRC32 (non-C) for checksums** — CRC32C wins on hardware acceleration + ubiquity; AEAD already covers cryptographic integrity.
- **Update-in-place vectors** — rejected: breaks immutability, snapshotting, and crash-safety.
