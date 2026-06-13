# ADR-0019: Disk-resident index format (DiskANN on encrypted pages)

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Phase 2's headline is memory frugality (ADR-0007): serve large collections from a
small RAM budget. The Vamana graph (ADR-0007) plus product quantization
(ADR-0008) make this possible — keep PQ codes in RAM to navigate, keep the graph
and full-precision vectors on SSD, and re-rank the candidates with exact
distances read from disk. This ADR fixes how that on-disk index is laid out, and
how it stays **encrypted at rest** like every other durable byte (owner
directive; ADR-0010, `docs/security/crypto.md`).

The tension: DiskANN wants "one random read per hop" with the node's adjacency
*and* its vector co-located, but Quiver's durability/crypto unit is the 16 KiB
[`PageCodec`] page (ADR-0004), not an arbitrary node block. Reconciling the two
must not reintroduce the plaintext-on-disk gap that the WAL nearly shipped with
in Phase 1.

## Decision

- **Reuse the page as the I/O + crypto unit.** A disk index is a sequence of
  16 KiB pages built and validated with the existing `build_page` / `parse_page`
  (CRC32C) and sealed with the same `PageCodec` the store uses — `PlainCodec`
  when encryption is off, the `quiver-crypto` AEAD codec when on. A new
  `PageType::IndexBlock` distinguishes index pages from manifest/segment pages so
  a page can never be silently misread across kinds.
- **Node blocks are packed into pages, vector and adjacency co-located.** Each
  node block is a fixed stride — `[vector: dim×4][neighbor_count: u32][neighbors:
  R×u32]` — and `floor(PAGE_BODY_CAP / stride)` nodes pack into one page. Reading
  a node decrypts its single containing page and slices the block out: the
  vector and its out-neighbors arrive together, so the exact-distance re-rank
  vector comes "for free" with the adjacency read.
- **File regions:** `[meta: 1 page][codebook: C pages][PQ codes: K pages][node
  blocks: B pages]`. The meta page records every count and region size. On open,
  the meta, the PQ codebook, and the **PQ codes are read into RAM** (the codes
  are the only per-vector data that must be resident); the node-block region is
  **`mmap`-ed** and decrypted on demand, so only the working set is resident.
- **Search = PQ-navigated beam search + exact re-rank.** Navigation ranks
  candidates by the RAM-resident PQ codes (cheap, no disk); expanding a node
  reads (and decrypts) its page for neighbors and its full vector; the visited
  nodes' full vectors then drive an exact-distance re-rank to the final top-k.
- **`quiver-index` depends on `quiver-core`** for the page primitives and the
  `PageCodec` trait. The disk index *is* an on-disk format, so it legitimately
  builds on the storage layer's page/crypto primitives rather than duplicating
  them; the dependency is acyclic (`core ← index ← embed`).

## Consequences

- **+** One encryption/integrity mechanism for the whole system; no bespoke
  crypto for the index, no plaintext-on-disk gap. Memory stays frugal: RAM holds
  PQ codes + the OS-resident working set of decrypted pages.
- **+** The immutable, page-structured file is snapshot- and backup-friendly like
  segments (ADR-0004).
- **−** A decrypt runs per page touched during navigation (the security tax over
  a plain `mmap` read). A decrypted-page cache is an obvious optimization, left
  for a later PR; the OS page cache already holds the ciphertext pages.
- **−** A node block must fit one page (`stride ≤ PAGE_BODY_CAP`, ~16 KiB), so a
  single page holds vectors up to a few thousand dimensions at typical `R`.
  Multi-page node blocks for very high dimensions are deferred.
- **−** `mmap` requires one `unsafe` call; justified because the index file is an
  immutable artifact (never mutated after write), with a `// SAFETY:` note.

## Alternatives considered

- **A bespoke un-paged DiskANN file `mmap`-ed in the clear** — rejected: fastest,
  but plaintext on disk violates the secure-by-default directive.
- **Per-node AEAD blocks (not page-packed)** — rejected: 16 KiB-granular sealing
  reuses the audited `PageCodec` untouched and amortizes the nonce/tag overhead
  across the several nodes that share a page.
- **`pread` instead of `mmap`** — viable and `unsafe`-free, same frugality, but
  the design (`docs/storage/on-disk-format.md`) specifies `mmap`; kept as a
  fallback if `mmap` portability ever bites.
