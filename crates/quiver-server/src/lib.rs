// SPDX-License-Identifier: AGPL-3.0-only
//! The Quiver daemon: gRPC and REST APIs over the embeddable handle, with
//! authentication, RBAC, rate limiting, auditing, and observability.
//!
//! Status: scaffolding — the server lands in Phase 1. Design:
//! `docs/api/rest-grpc.md`, ADR-0011 (authn/z), ADR-0013 (config), ADR-0014.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
