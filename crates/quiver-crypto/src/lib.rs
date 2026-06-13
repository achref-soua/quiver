// SPDX-License-Identifier: AGPL-3.0-only
//! Audited-cryptography wrappers for Quiver: envelope encryption, AEAD page
//! sealing, key derivation, and TLS configuration.
//!
//! Quiver implements no cryptographic primitives of its own — every primitive
//! comes from an audited library (`rustls`, RustCrypto / `ring`).
//!
//! Status: scaffolding — implementation lands in Phase 1. Design:
//! `docs/security/crypto.md`.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {}
}
