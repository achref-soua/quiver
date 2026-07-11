// SPDX-License-Identifier: AGPL-3.0-only
//! The embedded index's internal-id ↔ external-id map (ADR-0073), split into an
//! on-disk base and a small resident tail so the id strings leave the heap.
//!
//! The base — the live ids as of the last index rebuild, in `store.scan()` order —
//! is an on-demand-paged [`ExtIdColumn`]: the id strings live on disk, and only
//! ~12 B/pt of fixed-width `offsets`+`sorted` metadata is resident. The tail —
//! writes since that rebuild — stays in RAM as `tail` (internal→ext) + `overlay`
//! (ext→internal). Together they answer the read path (internal→ext, resolving a
//! query hit) and the write path (ext→internal, the upsert/delete oracle). A
//! rebuild reseals the base and resets the tail; the map is a rebuildable cache,
//! never the source of truth.
#![allow(clippy::wildcard_imports)]

use super::*;

/// The base (on-disk) + tail (resident) id map for one collection's index.
pub(crate) struct IdMap {
    // On-disk base sealed at the last rebuild; `None` before the first build. Shared
    // by `Arc` with the MVCC serving snapshot (both read-only), so a republish is
    // O(1), not a clone of the base ids.
    base: Option<Arc<ExtIdColumn>>,
    base_count: u64,
    // Internal ids `base_count..`: writes since the last rebuild. Resident, bounded
    // by the rebuild cadence (except the pathological never-rebuilt incremental IVF
    // — the named ceiling in ADR-0073).
    tail: Vec<String>,
    // ext → internal for the tail and for base ids shadowed this window; an entry
    // here wins over the base column's `lookup`. Append/overwrite only — a delete
    // keeps the mapping (the index tombstone hides the row), matching the pre-split
    // append-only `int_to_ext`/`ext_to_int`.
    overlay: HashMap<String, u64>,
}

impl IdMap {
    /// A fresh, empty map (no base yet); the first rebuild seals a base.
    pub(crate) fn empty() -> Self {
        Self {
            base: None,
            base_count: 0,
            tail: Vec::new(),
            overlay: HashMap::new(),
        }
    }

    /// A fresh map over a just-sealed base column: no tail, no overlay.
    pub(crate) fn from_base(col: ExtIdColumn) -> Self {
        Self {
            base_count: col.len(),
            base: Some(Arc::new(col)),
            tail: Vec::new(),
            overlay: HashMap::new(),
        }
    }

    /// Restore from an opened base column plus the resident `tail` persisted in the
    /// envelope; the overlay is rebuilt from the tail's ext→internal.
    pub(crate) fn restore(col: ExtIdColumn, tail: Vec<String>) -> Self {
        let base_count = col.len();
        let overlay = tail
            .iter()
            .enumerate()
            .map(|(j, ext)| (ext.clone(), base_count + j as u64))
            .collect();
        Self {
            base: Some(Arc::new(col)),
            base_count,
            tail,
            overlay,
        }
    }

    /// Total live-plus-shadowed internal ids (base + tail).
    pub(crate) fn len(&self) -> u64 {
        self.base_count + self.tail.len() as u64
    }

    /// Ids in the on-disk base (internal ids `0..base_count`).
    pub(crate) fn base_count(&self) -> u64 {
        self.base_count
    }

    /// The shared base column, if any — cloned into the MVCC snapshot.
    pub(crate) fn base(&self) -> Option<Arc<ExtIdColumn>> {
        self.base.clone()
    }

    /// The resident tail (internal ids `base_count..`), persisted in the envelope.
    pub(crate) fn tail(&self) -> &[String] {
        &self.tail
    }

    /// The internal id the next [`push`](Self::push) will assign.
    pub(crate) fn next_internal(&self) -> u64 {
        self.len()
    }

    /// internal → ext (read path): one page decrypt for a base id, a slice index for
    /// the tail. `None` for an out-of-range id.
    pub(crate) fn ext(&self, internal: u64) -> Result<Option<String>> {
        if internal < self.base_count {
            match &self.base {
                Some(c) => Ok(Some(c.read(internal)?)),
                None => Ok(None),
            }
        } else {
            Ok(self
                .tail
                .get((internal - self.base_count) as usize)
                .cloned())
        }
    }

    /// ext → internal (write path): the overlay (tail + base-shadows) wins over the
    /// base column's binary-search lookup.
    pub(crate) fn internal(&self, ext: &str) -> Result<Option<u64>> {
        if let Some(&i) = self.overlay.get(ext) {
            return Ok(Some(i));
        }
        match &self.base {
            Some(c) => Ok(c.lookup(ext)?),
            None => Ok(None),
        }
    }

    /// Append a new internal id for `ext` (a new or shadowing upsert) and return it.
    pub(crate) fn push(&mut self, ext: &str) -> u64 {
        let internal = self.len();
        self.tail.push(ext.to_owned());
        self.overlay.insert(ext.to_owned(), internal);
        internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_core::page::PlainCodec;

    // Build a real on-disk base column so the base read/lookup paths are exercised,
    // not just the resident tail.
    fn base_of(dir: &std::path::Path, ids: &[&str]) -> ExtIdColumn {
        let path = dir.join("idmap.qic");
        let owned: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
        ExtIdColumn::write(&path, &PlainCodec, &owned).unwrap();
        ExtIdColumn::open(&path, Box::new(PlainCodec)).unwrap()
    }

    #[test]
    fn base_then_tail_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = IdMap::from_base(base_of(dir.path(), &["a", "b", "c"]));
        assert_eq!(m.len(), 3);
        assert_eq!(m.base_count(), 3);

        // read path: base ids decrypt from the column, in internal order.
        assert_eq!(m.ext(0).unwrap().as_deref(), Some("a"));
        assert_eq!(m.ext(2).unwrap().as_deref(), Some("c"));
        assert_eq!(m.ext(9).unwrap(), None);
        // write path: base ids resolve via the column's binary search.
        assert_eq!(m.internal("b").unwrap(), Some(1));
        assert_eq!(m.internal("a").unwrap(), Some(0));
        assert_eq!(m.internal("zzz").unwrap(), None);

        // a new upsert appends to the resident tail.
        assert_eq!(m.next_internal(), 3);
        assert_eq!(m.push("d"), 3);
        assert_eq!(m.len(), 4);
        assert_eq!(m.ext(3).unwrap().as_deref(), Some("d")); // tail read
        assert_eq!(m.internal("d").unwrap(), Some(3));
        assert_eq!(m.tail(), &["d".to_owned()]);

        // shadowing a base id: the overlay entry wins over the base lookup, and a
        // fresh internal id is handed out (the base copy is tombstoned by the index).
        assert_eq!(m.push("a"), 4);
        assert_eq!(m.internal("a").unwrap(), Some(4));
        // the stale base copy is still readable at its old internal id (the index
        // never returns it, so resolution never asks) — proves no corruption.
        assert_eq!(m.ext(0).unwrap().as_deref(), Some("a"));
    }

    #[test]
    fn restore_rebuilds_overlay_from_tail() {
        let dir = tempfile::tempdir().unwrap();
        let m = IdMap::restore(
            base_of(dir.path(), &["a", "b"]),
            vec!["c".into(), "d".into()],
        );
        assert_eq!(m.len(), 4);
        assert_eq!(m.internal("a").unwrap(), Some(0)); // base
        assert_eq!(m.internal("c").unwrap(), Some(2)); // tail via rebuilt overlay
        assert_eq!(m.internal("d").unwrap(), Some(3));
        assert_eq!(m.ext(3).unwrap().as_deref(), Some("d"));
    }

    #[test]
    fn empty_map() {
        let mut m = IdMap::empty();
        assert_eq!(m.len(), 0);
        assert_eq!(m.internal("x").unwrap(), None);
        assert_eq!(m.ext(0).unwrap(), None);
        assert_eq!(m.push("x"), 0);
        assert_eq!(m.internal("x").unwrap(), Some(0));
    }
}
