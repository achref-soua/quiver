// SPDX-License-Identifier: AGPL-3.0-only
//! The committed OpenAPI spec must document exactly the REST surface the router
//! serves — no fiction, no gaps. A hand-written spec can drift from the code, so
//! this test pins the two together: every path the router registers appears in
//! `docs/api/openapi.yaml`, and the endpoints that were removed as fiction stay
//! out. Keep the `ROUTER_PATHS` list in sync with `rest.rs::router`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

const SPEC: &str = include_str!("../../../docs/api/openapi.yaml");

// The path templates registered in `crates/quiver-server/src/rest.rs::router`.
const ROUTER_PATHS: &[&str] = &[
    "/v1/collections",
    "/v1/collections/{name}",
    "/v1/collections/{name}/points",
    "/v1/collections/{name}/points:bulk",
    "/v1/collections/{name}/points:text",
    "/v1/collections/{name}/points/{id}",
    "/v1/collections/{name}/query",
    "/v1/collections/{name}/query/hybrid",
    "/v1/collections/{name}/query/text",
    "/v1/collections/{name}/fetch",
    "/v1/collections/{name}/documents",
    "/v1/collections/{name}/documents/query",
    "/v1/snapshot",
    "/cluster/map",
    "/cluster/raft/voters",
    "/cluster/raft/voters/{id}",
    "/healthz",
    "/readyz",
    "/metrics",
];

#[test]
fn openapi_documents_every_rest_route() {
    for p in ROUTER_PATHS {
        // A path key in the spec is `  <path>:` at two-space indent.
        assert!(
            SPEC.contains(&format!("\n  {p}:")),
            "openapi.yaml is missing the route {p}"
        );
    }
}

#[test]
fn openapi_does_not_document_unimplemented_endpoints() {
    // These were documented in rest-grpc.md but never implemented; the spec must
    // not resurrect them.
    for fiction in ["/query/batch", "/v1/keys", "/stats"] {
        assert!(
            !SPEC.contains(fiction),
            "openapi.yaml documents the unimplemented endpoint {fiction}"
        );
    }
}
