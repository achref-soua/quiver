// SPDX-License-Identifier: AGPL-3.0-only
//! Collection descriptors: the schema fixed when a collection is created.

use serde::{Deserialize, Serialize};

/// The element type of stored vectors. Phase 1 ships `f32`; lower-precision and
/// quantized dtypes arrive with the memory-frugality work in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Dtype {
    /// 32-bit IEEE-754 float.
    F32,
}

impl Dtype {
    /// Size in bytes of one vector element.
    #[must_use]
    pub const fn element_size(self) -> usize {
        match self {
            Dtype::F32 => 4,
        }
    }
}

/// The distance / similarity function a collection is searched with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistanceMetric {
    /// Inner product — higher is more similar.
    Dot,
    /// Cosine similarity — higher is more similar.
    Cosine,
    /// Squared Euclidean distance — lower is more similar.
    L2,
}

/// The index structure a collection is served by (ADR-0007). The default is the
/// in-memory HNSW graph; the others are the Phase 2 memory-frugal options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IndexKind {
    /// In-memory HNSW graph: lowest latency, fits in RAM. The default.
    #[default]
    Hnsw,
    /// In-memory Vamana (DiskANN) graph.
    Vamana,
    /// Disk-resident Vamana: PQ codes in RAM, graph + full vectors on SSD.
    DiskVamana,
    /// Inverted-file index with coarse Voronoi partitioning.
    Ivf,
}

/// Which index a collection uses and how its vectors are compressed (ADR-0007,
/// ADR-0008). Defaults to in-memory HNSW with no quantization (exact search).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IndexSpec {
    /// The index structure.
    pub kind: IndexKind,
    /// Product-quantization subspaces for quantized kinds (the disk graph,
    /// IVF+PQ). `None` selects a kind-appropriate default or no quantization.
    pub pq_subspaces: Option<u32>,
}

/// The immutable schema of a collection, fixed at creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    /// Vector dimensionality.
    pub dim: u32,
    /// Vector element type.
    pub dtype: Dtype,
    /// Distance metric used for search.
    pub metric: DistanceMetric,
    /// Index & quantization configuration. Defaults to HNSW/exact and is absent
    /// in descriptors written before Phase 2 (filled by the default on read).
    #[serde(default)]
    pub index: IndexSpec,
}

impl Descriptor {
    /// A descriptor with the default index (in-memory HNSW, exact).
    #[must_use]
    pub fn new(dim: u32, dtype: Dtype, metric: DistanceMetric) -> Self {
        Self {
            dim,
            dtype,
            metric,
            index: IndexSpec::default(),
        }
    }

    /// Set the index specification (builder style).
    #[must_use]
    pub fn with_index(mut self, index: IndexSpec) -> Self {
        self.index = index;
        self
    }

    /// Decode a descriptor from its postcard bytes, tolerating the pre-Phase-2
    /// layout that had no `index` field.
    ///
    /// postcard is non-self-describing, so a missing *trailing* field cannot be
    /// defaulted by `#[serde(default)]` alone (the reader hits end-of-input and
    /// errors). We therefore try the current layout first and fall back to the
    /// legacy three-field layout, defaulting the index to HNSW.
    ///
    /// # Errors
    /// Returns the postcard error if the bytes match neither layout.
    pub fn decode(bytes: &[u8]) -> std::result::Result<Self, postcard::Error> {
        postcard::from_bytes::<Self>(bytes)
            .or_else(|_| postcard::from_bytes::<LegacyDescriptor>(bytes).map(Self::from))
    }

    /// Byte length of one stored vector (`dim × element_size`).
    #[must_use]
    pub fn stride(&self) -> usize {
        self.dim as usize * self.dtype.element_size()
    }
}

// The pre-Phase-2 on-disk descriptor layout (no `index` field), kept only to
// migrate older databases on read via [`Descriptor::decode`].
#[derive(Deserialize)]
struct LegacyDescriptor {
    dim: u32,
    dtype: Dtype,
    metric: DistanceMetric,
}

impl From<LegacyDescriptor> for Descriptor {
    fn from(v: LegacyDescriptor) -> Self {
        Self {
            dim: v.dim,
            dtype: v.dtype,
            metric: v.metric,
            index: IndexSpec::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stride_matches_dim_and_dtype() {
        let d = Descriptor::new(128, Dtype::F32, DistanceMetric::L2);
        assert_eq!(d.stride(), 512);
        assert_eq!(Dtype::F32.element_size(), 4);
        // The default index is in-memory HNSW with no quantization.
        assert_eq!(d.index, IndexSpec::default());
        assert_eq!(d.index.kind, IndexKind::Hnsw);
    }

    #[test]
    fn descriptor_roundtrips_through_postcard() {
        let d = Descriptor::new(8, Dtype::F32, DistanceMetric::Cosine).with_index(IndexSpec {
            kind: IndexKind::DiskVamana,
            pq_subspaces: Some(16),
        });
        let bytes = postcard::to_allocvec(&d).unwrap();
        let back: Descriptor = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(d, back);
    }

    // A descriptor serialized before the `index` field existed (only dim, dtype,
    // metric) must still deserialize, defaulting the index to HNSW.
    #[test]
    fn pre_phase2_descriptor_deserializes_with_default_index() {
        #[derive(serde::Serialize)]
        struct OldDescriptor {
            dim: u32,
            dtype: Dtype,
            metric: DistanceMetric,
        }
        let old = OldDescriptor {
            dim: 16,
            dtype: Dtype::F32,
            metric: DistanceMetric::L2,
        };
        let bytes = postcard::to_allocvec(&old).unwrap();
        // The raw new-layout decode fails on the shorter legacy bytes...
        assert!(postcard::from_bytes::<Descriptor>(&bytes).is_err());
        // ...but `decode` falls back to the legacy layout and defaults the index.
        let back = Descriptor::decode(&bytes).unwrap();
        assert_eq!(back.dim, 16);
        assert_eq!(back.metric, DistanceMetric::L2);
        assert_eq!(back.index, IndexSpec::default());
    }

    #[test]
    fn decode_reads_current_layout() {
        let d = Descriptor::new(8, Dtype::F32, DistanceMetric::Dot).with_index(IndexSpec {
            kind: IndexKind::Ivf,
            pq_subspaces: Some(8),
        });
        let bytes = postcard::to_allocvec(&d).unwrap();
        assert_eq!(Descriptor::decode(&bytes).unwrap(), d);
    }
}
