// SPDX-License-Identifier: AGPL-3.0-only
//! Generated wire types and gRPC service stubs for Quiver (`tonic`/`prost`).
//!
//! The `.proto` in `proto/quiver.proto` is the source of truth (ADR-0018); this
//! crate compiles it at build time and re-exports the generated client, server,
//! and message types under [`v1`]. Design: `docs/api/wire-protocol.md`,
//! `docs/api/rest-grpc.md`.

/// The `quiver.v1` package: generated messages, `quiver_client`, and
/// `quiver_server`.
pub mod v1 {
    // Generated code is trusted; suppress the workspace's strict lints for it.
    #![allow(
        missing_docs,
        clippy::all,
        clippy::pedantic,
        clippy::nursery,
        clippy::unwrap_used,
        clippy::expect_used
    )]
    include!(concat!(env!("OUT_DIR"), "/quiver.v1.rs"));
}

#[cfg(test)]
mod tests {
    use super::v1;

    #[test]
    fn generated_types_are_present() {
        // Construct a couple of generated messages to prove codegen ran.
        let req = v1::CreateCollectionRequest {
            name: "demo".to_owned(),
            dim: 8,
            metric: v1::Metric::L2 as i32,
            index: v1::IndexKind::DiskVamana as i32,
            pq_subspaces: Some(4),
        };
        assert_eq!(req.dim, 8);
        assert_eq!(req.index, v1::IndexKind::DiskVamana as i32);
        let resp = v1::SearchResponse::default();
        assert!(resp.matches.is_empty());
    }
}
