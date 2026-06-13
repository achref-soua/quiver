# ADR-0010: Crypto — envelope encryption & AEAD

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Encryption-at-rest must be on by default with secure defaults, cover all durable data (including index artifacts), support key rotation and crypto-shredding, and use only audited cryptography. The mechanism is detailed in [`../security/crypto.md`](../security/crypto.md).

## Decision

- **Audited libraries only:** `rustls` (TLS), RustCrypto/`ring` (AEAD, HKDF, key wrap). **No home-grown primitives.**
- **Envelope encryption:** Master Key → wraps per-collection **DEK** → derives **per-page subkeys** via `HKDF-SHA256(DEK, file_id‖page_id‖page_version)`, so **nonce reuse is impossible by construction**.
- **AEAD:** **AES-256-GCM** by default when AES-NI is detected, else **ChaCha20-Poly1305**; algorithm id recorded in the collection key metadata. DEK wrapping uses AES-256-GCM-SIV (nonce-misuse-resistant) or the KMS.
- **Secure by default:** encryption-at-rest **on** out of the box; MK from a `0600` file or external **KMS**, never on disk in plaintext; DEKs zeroized in memory (`zeroize`).
- **Crypto-shredding:** destroy a collection's wrapped DEK ⇒ its data (and backups) is unrecoverable.

## Consequences

- **+** Strong, standard at-rest protection covering data and index files; per-collection erasure; no nonce-reuse footgun; KMS-ready.
- **−** Per-page subkey derivation costs an HKDF per page (cheap, amortized over a 16 KiB page; cacheable per page-version). Key rotation re-wraps DEKs (cheap) — re-encrypting data under a new DEK is a background re-write (documented).

## Alternatives considered

- **Single key, random 96-bit GCM nonces** — rejected: birthday-bound nonce-collision risk at database scale is unacceptable for GCM.
- **Full-disk/file-system encryption only** — rejected: coarser, not per-collection, no crypto-shredding, outside the app's control.
- **Rolling our own cipher/mode** — categorically rejected (security-first non-negotiable).
