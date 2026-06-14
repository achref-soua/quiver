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

/// The type of a filterable payload field, which fixes how its values are keyed
/// in the secondary index (`.sec`) — and therefore which predicates it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FieldType {
    /// An exact-match string field (equality and lexical range).
    Keyword,
    /// A numeric field (equality and numeric range), keyed order-preserving.
    Numeric,
}

/// A payload field declared filterable at collection creation: its dot-path and
/// type. Declared fields are extracted into the per-segment secondary index at
/// flush time (ADR-0022), enabling pre-filtered (hybrid) search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterableField {
    /// Dot-path into the JSON payload (e.g. `"user.age"`).
    pub path: String,
    /// The field's value type.
    pub field_type: FieldType,
}

impl FilterableField {
    /// A keyword (exact-match string) field at `path`.
    #[must_use]
    pub fn keyword(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            field_type: FieldType::Keyword,
        }
    }

    /// A numeric field at `path`.
    #[must_use]
    pub fn numeric(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            field_type: FieldType::Numeric,
        }
    }
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
    /// Payload fields indexed for filtering. Empty by default and absent in
    /// descriptors written before secondary indexes existed (defaulted on read).
    #[serde(default)]
    pub filterable: Vec<FilterableField>,
}

impl Descriptor {
    /// A descriptor with the default index (in-memory HNSW, exact) and no
    /// filterable fields.
    #[must_use]
    pub fn new(dim: u32, dtype: Dtype, metric: DistanceMetric) -> Self {
        Self {
            dim,
            dtype,
            metric,
            index: IndexSpec::default(),
            filterable: Vec::new(),
        }
    }

    /// Set the index specification (builder style).
    #[must_use]
    pub fn with_index(mut self, index: IndexSpec) -> Self {
        self.index = index;
        self
    }

    /// Set the filterable payload fields (builder style).
    #[must_use]
    pub fn with_filterable(mut self, filterable: Vec<FilterableField>) -> Self {
        self.filterable = filterable;
        self
    }

    /// Decode a descriptor from its postcard bytes, tolerating every earlier
    /// layout.
    ///
    /// postcard is non-self-describing, so a missing *trailing* field cannot be
    /// defaulted by `#[serde(default)]` alone (the reader hits end-of-input and
    /// errors). We therefore try the layouts newest-to-oldest — current
    /// (with `filterable`) → the four-field `index`-only layout → the original
    /// three-field layout — defaulting the missing trailing fields. The order
    /// matters: postcard ignores trailing bytes, so an older decoder would
    /// silently mis-read a newer buffer if tried first.
    ///
    /// # Errors
    /// Returns the postcard error if the bytes match no known layout.
    pub fn decode(bytes: &[u8]) -> std::result::Result<Self, postcard::Error> {
        postcard::from_bytes::<Self>(bytes)
            .or_else(|_| postcard::from_bytes::<DescriptorV2>(bytes).map(Self::from))
            .or_else(|_| postcard::from_bytes::<LegacyDescriptor>(bytes).map(Self::from))
    }

    /// Byte length of one stored vector (`dim × element_size`).
    #[must_use]
    pub fn stride(&self) -> usize {
        self.dim as usize * self.dtype.element_size()
    }
}

// The four-field layout (an `index` but no `filterable`), kept only to migrate
// descriptors written before secondary indexes existed, via [`Descriptor::decode`].
#[derive(Deserialize)]
struct DescriptorV2 {
    dim: u32,
    dtype: Dtype,
    metric: DistanceMetric,
    index: IndexSpec,
}

impl From<DescriptorV2> for Descriptor {
    fn from(v: DescriptorV2) -> Self {
        Self {
            dim: v.dim,
            dtype: v.dtype,
            metric: v.metric,
            index: v.index,
            filterable: Vec::new(),
        }
    }
}

// The original three-field layout (no `index`, no `filterable`), kept only to
// migrate the oldest databases on read via [`Descriptor::decode`].
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
            filterable: Vec::new(),
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

    // A descriptor serialized before `filterable` existed (four fields, with an
    // `index`) must still deserialize — and the four-field fallback must run
    // before the three-field one, so the `index` is preserved, not defaulted.
    #[test]
    fn pre_filterable_descriptor_decodes_and_keeps_its_index() {
        #[derive(serde::Serialize)]
        struct DescriptorV2 {
            dim: u32,
            dtype: Dtype,
            metric: DistanceMetric,
            index: IndexSpec,
        }
        let old = DescriptorV2 {
            dim: 8,
            dtype: Dtype::F32,
            metric: DistanceMetric::L2,
            index: IndexSpec {
                kind: IndexKind::DiskVamana,
                pq_subspaces: Some(16),
            },
        };
        let bytes = postcard::to_allocvec(&old).unwrap();
        // The current five-field decode fails on the shorter buffer...
        assert!(postcard::from_bytes::<Descriptor>(&bytes).is_err());
        // ...but `decode` falls back to the four-field layout, keeping the index
        // (not the three-field legacy layout, which would lose it).
        let back = Descriptor::decode(&bytes).unwrap();
        assert_eq!(back.dim, 8);
        assert_eq!(back.index.kind, IndexKind::DiskVamana);
        assert_eq!(back.index.pq_subspaces, Some(16));
        assert!(back.filterable.is_empty());
    }

    #[test]
    fn descriptor_with_filterable_roundtrips() {
        let d = Descriptor::new(4, Dtype::F32, DistanceMetric::L2).with_filterable(vec![
            FilterableField::keyword("city"),
            FilterableField::numeric("age"),
        ]);
        let bytes = postcard::to_allocvec(&d).unwrap();
        assert_eq!(Descriptor::decode(&bytes).unwrap(), d);
    }
}
