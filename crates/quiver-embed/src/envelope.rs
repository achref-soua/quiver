// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk index-snapshot envelopes (ADR-0025): the durable IVF and DiskVamana
//! blobs the store persists and reloads. Split out of the crate root; re-exported
//! by `lib.rs`, so no reference elsewhere changes.
#![allow(clippy::wildcard_imports)]

use super::*;

/// On-disk envelope (ADR-0025) for a durable IVF snapshot: the `Ivf` bytes plus
/// the internal->external id mapping they are addressed by, postcard-encoded and
/// handed to the store as one opaque blob. On open the envelope is decoded, the
/// `Ivf` restored, and the post-checkpoint WAL tail replayed. A decode/version
/// error means "rebuild from the store" — the snapshot is only ever a fast path.
#[derive(Serialize, Deserialize)]
pub(crate) struct IndexEnvelope {
    pub(crate) version: u16,
    pub(crate) int_to_ext: Vec<String>,
    pub(crate) ivf: Vec<u8>,
}

// Envelope format version, independent of the product SemVer (and of the inner
// `Ivf` snapshot version); a mismatch falls back to a rebuild.
pub(crate) const INDEX_ENVELOPE_VERSION: u16 = 1;

/// On-disk envelope (ADR-0063) for a durable DiskVamana snapshot. Unlike the IVF
/// envelope, the bulk (graph + full vectors) stays in the immutable `mmap`-ed
/// base file (`vamana.qvx`); this blob carries only what ties that base to the
/// live state — the base point count (validated against the opened file), the
/// FreshDiskANN tombstones, and the id map. The delta vectors are *not* stored:
/// the delta ids are implied as `[base_row_count, int_to_ext.len())` and their
/// vectors re-fetched from the store on open, so the blob stays O(delta ids), not
/// O(N) vectors. A decode/version/validation error means "rebuild from the store".
#[derive(Serialize, Deserialize)]
pub(crate) struct DiskEnvelope {
    pub(crate) version: u16,
    pub(crate) int_to_ext: Vec<String>,
    pub(crate) base_row_count: u64,
    pub(crate) deleted_ids: Vec<u64>,
}
