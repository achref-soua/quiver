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

/// The immutable schema of a collection, fixed at creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    /// Vector dimensionality.
    pub dim: u32,
    /// Vector element type.
    pub dtype: Dtype,
    /// Distance metric used for search.
    pub metric: DistanceMetric,
}

impl Descriptor {
    /// Byte length of one stored vector (`dim × element_size`).
    #[must_use]
    pub fn stride(&self) -> usize {
        self.dim as usize * self.dtype.element_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stride_matches_dim_and_dtype() {
        let d = Descriptor {
            dim: 128,
            dtype: Dtype::F32,
            metric: DistanceMetric::L2,
        };
        assert_eq!(d.stride(), 512);
        assert_eq!(Dtype::F32.element_size(), 4);
    }

    #[test]
    fn descriptor_roundtrips_through_postcard() {
        let d = Descriptor {
            dim: 8,
            dtype: Dtype::F32,
            metric: DistanceMetric::Cosine,
        };
        let bytes = postcard::to_allocvec(&d).unwrap();
        let back: Descriptor = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(d, back);
    }
}
