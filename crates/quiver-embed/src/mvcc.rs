// SPDX-License-Identifier: AGPL-3.0-only
//! The single-writer MVCC write path (ADR-0064): publishing snapshots and applying
//! overlay upserts/deletes/tombstones. Split out of the crate root for review; all
//! items are re-exported by `lib.rs`, so no reference elsewhere changes.
#![allow(clippy::wildcard_imports)]

use super::*;

// Whether a collection *kind* can be served by the lock-free MVCC snapshot path
// (ADR-0064 increment 1), independent of the flag: single-vector, server-searchable,
// and an **in-memory** index. Disk-resident indexes are excluded for now — their
// `mmap` assumes an immutable file, which a superseded snapshot still referencing
// the old file would violate when a rebuild seals a new one (a later increment
// versions the file).
pub(crate) fn mvcc_eligible(descriptor: &Descriptor) -> bool {
    !descriptor.multivector
        && descriptor.vector_encryption != VectorEncryption::ClientSide
        && descriptor.index.kind != IndexKind::DiskVamana
}

// Whether a collection is *currently* served via the snapshot: eligible and the
// flag is on.
pub(crate) fn mvcc_served(handle: &CollectionHandle) -> bool {
    handle.mvcc && mvcc_eligible(&handle.descriptor)
}

// Republish a collection's snapshot reusing the immutable base + id map, swapping
// in a freshly-extended overlay (the per-write O(overlay) cost).
pub(crate) fn publish_overlay(
    handle: &CollectionHandle,
    prior: &CollectionSnapshot,
    overlay: Arc<Overlay>,
) {
    handle.snapshot.store(Arc::new(CollectionSnapshot {
        base: prior.base.clone(),
        base_int_to_ext: prior.base_int_to_ext.clone(),
        base_len: prior.base_len,
        overlay,
        metric: prior.metric,
    }));
}

// Move a freshly rebuilt index out of `handle.index` into a new published
// snapshot with an empty overlay (the base now absorbs all prior writes). Leaves
// `handle.index` empty — in MVCC mode the live base lives in the snapshot.
pub(crate) fn publish_base(handle: &mut CollectionHandle) {
    let base = std::mem::replace(&mut handle.index, empty_index(&handle.descriptor));
    let metric = to_index_metric(handle.descriptor.metric);
    handle.snapshot.store(Arc::new(CollectionSnapshot {
        base: Arc::new(base),
        base_int_to_ext: Arc::new(handle.int_to_ext.clone()),
        base_len: handle.int_to_ext.len() as u64,
        overlay: Arc::new(Overlay::default()),
        metric,
    }));
}

// Overlay size at which an MVCC write defers a consolidating rebuild: ~20% churn
// over the base (the same threshold that already triggers consolidation), with a
// floor so a tiny base does not rebuild on every write.
pub(crate) fn overlay_rebuild_threshold(base_len: u64) -> u64 {
    (base_len / 5).max(1024)
}

// MVCC-mode single-vector upsert (ADR-0064): append to the published overlay
// instead of mutating the immutable base, so lock-free readers stay race-free.
// A batch upsert coalesces the per-write clone-and-publish via
// [`overlay_upsert_batch`].
pub(crate) fn overlay_upsert(handle: &mut CollectionHandle, ext_id: &str, vector: &[f32]) {
    overlay_upsert_batch(handle, std::iter::once((ext_id, vector)));
}

// MVCC-mode batched single-vector upsert (ADR-0064): apply the whole batch to one
// cloned overlay and publish it once, instead of cloning + republishing the
// (growing) overlay per point — O(overlay) once, not O(n·overlay). Building on one
// snapshot means the batch also becomes visible atomically, as a single published
// snapshot. The single writer makes this safe. Identical to calling
// [`overlay_upsert`] per point: internal ids are assigned in order from the fixed
// base length, and an in-batch update tombstones the prior copy (the running
// `ext_to_int` reflects earlier points in the same batch).
pub(crate) fn overlay_upsert_batch<'a>(
    handle: &mut CollectionHandle,
    points: impl IntoIterator<Item = (&'a str, &'a [f32])>,
) {
    bump_write_gen(handle);
    let cur = handle.snapshot.load_full();
    let mut overlay = cur.overlay.as_ref().clone();
    for (ext_id, vector) in points {
        // An update supersedes the prior copy (in the base or the overlay): tombstone it.
        if let Some(&old) = handle.ext_to_int.get(ext_id) {
            overlay.tombstones.insert(old);
        }
        let internal = cur.base_len + overlay.upserts.len() as u64;
        overlay.upserts.push((Arc::from(vector), ext_id.to_owned()));
        handle.ext_to_int.insert(ext_id.to_owned(), internal);
        handle.int_to_ext.push(ext_id.to_owned());
    }
    let crowded = overlay.upserts.len() as u64 >= overlay_rebuild_threshold(cur.base_len);
    publish_overlay(handle, &cur, Arc::new(overlay));
    // Defer a consolidating rebuild once the overlay is large (the server runs it
    // off-lock; the embedded `&mut` search rebuilds synchronously).
    if crowded {
        handle.stale = true;
    }
}

// MVCC-mode single-vector delete (ADR-0064): tombstone the id in the published
// overlay; the base stays immutable.
pub(crate) fn overlay_delete(handle: &mut CollectionHandle, ext_id: &str) {
    bump_write_gen(handle);
    let Some(&internal) = handle.ext_to_int.get(ext_id) else {
        return;
    };
    let cur = handle.snapshot.load_full();
    let mut overlay = cur.overlay.as_ref().clone();
    overlay.tombstones.insert(internal);
    publish_overlay(handle, &cur, Arc::new(overlay));
}
