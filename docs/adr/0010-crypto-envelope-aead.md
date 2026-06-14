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

## Implementation

- **Phase 1 (shipped):** a single operator key sealed all durable data through one [`AeadCodec`] (XChaCha20-Poly1305 + per-page/record HKDF-SHA256 subkeys, random 192-bit nonce) — segments, manifest, and the WAL. This remains the behaviour of `Store::open_with_codec`.
- **Envelope key-ring (shipped, Phase 3):** the engine now takes a [`KeyRing`] (`quiver-core`) that supplies a **catalog** codec (manifest + WAL) and a **per-collection** codec (segments + index). `quiver-crypto`'s [`EnvelopeKeyRing`] implements it as the two-level hierarchy: the configured key is the **master key (MK)**; each collection gets a random 256-bit **DEK**, sealed with an `AeadCodec` keyed by that DEK. DEKs are stored **wrapped** under the MK (XChaCha20-Poly1305, AAD-bound to the collection id) as `<data_dir>/keys/<id>.dek`, written atomically. The catalog codec is keyed by `HKDF-SHA256(MK, "catalog")` and the DEK-wrapping key by `HKDF-SHA256(MK, "dek-wrap")`, so the three uses are domain-separated. The MK never touches disk. The server now opens the encrypted path this way, so `QUIVER_ENCRYPTION_KEY` is the master key.
- **Crypto-shredding (shipped):** `Store::shred_collection` (and `Database::shred_collection`) drops the collection, checkpoints (sealing any un-checkpointed rows into DEK-protected segments and rotating the WAL), and deletes the wrapped DEK. The DEK existed only in that file, so the collection's sealed segments and index are then unrecoverable even to the MK holder and even if the ciphertext survives in a backup. A plain `drop_collection` also reclaims the DEK at the next checkpoint's GC. Proven in `quiver-crypto`'s `envelope.rs` unit tests and `tests/envelope_shred.rs`.
- **Honest deviations from the decision above:** the AEAD is **XChaCha20-Poly1305** throughout (not AES-256-GCM with AES-NI auto-select) — its extended random nonce removes the nonce-reuse footgun without a counter, and it matches the at-rest codec; AES-NI selection is a future option. DEK wrapping uses **XChaCha20-Poly1305** (not AES-256-GCM-SIV) for the same reason and to avoid an extra primitive. The MK source is `QUIVER_ENCRYPTION_KEY` today; a `0600` key file and external **KMS** are the next slice (`docs/security/crypto.md`).
- **Scope of erasure:** crypto-shredding covers a collection's durable **segments and index** (the bulk store). Recent un-checkpointed writes live in the catalog-keyed WAL; `shred_collection` checkpoints and rotates it first, so after it returns no live key decrypts the collection's durable data. A WAL *backup* captured before the shred is out of scope (the inherent caveat of any erase-before-backup).
- **Format change:** the per-collection envelope changes the at-rest key hierarchy from v0.2.0's single root key. Quiver is pre-1.0 with no at-rest migrator, so an encrypted store from v0.2.0 must be re-created under v0.3.0 (it will fail to open: the segments expect a per-collection DEK that the old store never wrote).

[`AeadCodec`]: ../../crates/quiver-crypto/src/codec.rs
[`EnvelopeKeyRing`]: ../../crates/quiver-crypto/src/envelope.rs
[`KeyRing`]: ../../crates/quiver-core/src/keyring.rs
