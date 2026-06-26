// SPDX-License-Identifier: AGPL-3.0-only
//! Vector indexes for Quiver, pluggable per collection (ADR-0007).
//!
//! Phase 1 ships an in-memory **HNSW** graph ([`Hnsw`]) — high recall, lowest
//! latency — behind the [`Index`] trait. Phase 2 adds the memory-frugal pieces:
//! the [`Quantizer`] implementations (scalar, product, binary) that compress
//! vectors for RAM-resident codes, with the disk-resident graph (DiskANN/Vamana)
//! and IVF landing behind the same [`Index`] trait. Distance math is delegated
//! to `quiver-simd`. Design: `docs/index/design.md`, ADR-0007, ADR-0008.

mod colbert;
pub mod disk;
mod fresh;
pub mod gpu;
mod hnsw;
mod ivf;
mod kmeans;
mod quant;
mod rng;
mod score;
mod vamana;

pub use colbert::{ColbertConfig, ColbertIndex};
pub use disk::{DiskError, DiskSearchParams, DiskVamana};
pub use fresh::{FreshDiskVamana, FreshVamana};
pub use hnsw::{Hnsw, HnswConfig};
pub use ivf::{Ivf, IvfConfig, SnapshotError};
pub use quant::{BinaryQuantizer, CodeScorer, ProductQuantizer, Quantizer, ScalarQuantizer};
pub use quiver_simd::Metric;
pub use score::{max_sim, ordering_distance, report_metric};
pub use vamana::{Vamana, VamanaConfig};

use thiserror::Error;

/// Errors returned by an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum IndexError {
    /// A query or inserted vector did not match the index dimensionality.
    #[error("vector has {got} dims, index expects {expected}")]
    DimensionMismatch {
        /// Dimensionality the index was built with.
        expected: usize,
        /// Dimensionality of the offending vector.
        got: usize,
    },
    /// A quantizer was configured with invalid parameters (e.g. a subspace
    /// count that does not divide the dimensionality).
    #[error("invalid quantizer configuration: {0}")]
    InvalidConfig(&'static str),
}

/// A search result: an external id and its distance under the index metric.
///
/// `distance` is the metric value: lower is closer for [`Metric::L2`]; higher is
/// closer for [`Metric::Dot`] and [`Metric::Cosine`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbor {
    /// The external id supplied at insert time.
    pub id: u64,
    /// Distance / similarity to the query under the index metric.
    pub distance: f32,
}

/// A pluggable vector index (ADR-0007): `insert` points, then `search` for the
/// `k` nearest to a query. `ef_search` trades recall for latency at query time.
pub trait Index {
    /// Insert a point under external id `id`. Ids are append-only in Phase 1;
    /// updates and deletes are handled by rebuilding from the store.
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<(), IndexError>;

    /// Return the `k` nearest neighbors to `query`, closest first. `ef_search`
    /// is the search beam width (clamped up to at least `k`).
    fn search(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
    ) -> Result<Vec<Neighbor>, IndexError>;

    /// Number of points in the index.
    fn len(&self) -> usize;

    /// Whether the index holds no points.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
