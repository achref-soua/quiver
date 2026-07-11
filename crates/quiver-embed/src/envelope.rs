// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk index-snapshot envelopes (ADR-0025): the durable IVF and DiskVamana
//! blobs the store persists and reloads. Split out of the crate root; re-exported
//! by `lib.rs`, so no reference elsewhere changes.
#![allow(clippy::wildcard_imports)]

use super::*;

/// On-disk envelope (ADR-0025) for a durable IVF snapshot: the `Ivf` bytes plus
/// the resident tail of the id map (ADR-0073). The base ids live on disk in the
/// `idmap.qic` column (sealed at the last rebuild); this blob carries only
/// `base_count` (validated against that column on open) and the post-rebuild
/// `tail`, postcard-encoded and handed to the store as one opaque blob. On open the
/// envelope is decoded, the `Ivf` restored, the base column reopened, and the
/// post-checkpoint WAL tail replayed. A decode/version error means "rebuild from
/// the store" — the snapshot is only ever a fast path.
#[derive(Serialize, Deserialize)]
pub(crate) struct IndexEnvelope {
    pub(crate) version: u16,
    pub(crate) base_count: u64,
    pub(crate) tail: Vec<String>,
    pub(crate) ivf: Vec<u8>,
}

// Envelope format version, independent of the product SemVer (and of the inner
// `Ivf` snapshot version); a mismatch falls back to a rebuild. Bumped to 2 when the
// base ids moved out of the blob into the on-disk `idmap.qic` column (ADR-0073).
pub(crate) const INDEX_ENVELOPE_VERSION: u16 = 2;

/// On-disk envelope (ADR-0063) for a durable DiskVamana snapshot. Like the IVF
/// envelope, the base ids live in the `idmap.qic` column and the graph + full
/// vectors in the immutable `mmap`-ed base file (`vamana.qvx`); this blob carries
/// only what ties those to the live state — `base_count` (validated against both
/// opened files), the FreshDiskANN tombstones, and the resident `tail`. The delta
/// vectors are *not* stored: the delta ids are exactly the `tail` (internal ids
/// `[base_count, base_count + tail.len())`) and their vectors re-fetched from the
/// store on open, so the blob stays O(delta ids), not O(N) vectors. A
/// decode/version/validation error means "rebuild from the store".
#[derive(Serialize, Deserialize)]
pub(crate) struct DiskEnvelope {
    pub(crate) version: u16,
    pub(crate) base_count: u64,
    pub(crate) tail: Vec<String>,
    pub(crate) deleted_ids: Vec<u64>,
}
