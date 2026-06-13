# Cryptography

**Non-negotiable: Quiver implements no cryptographic primitives or protocols of its own.** Every primitive comes from an audited library — `rustls` for TLS, and RustCrypto crates for AEAD, hashing, KDF, and key wrapping. Rolling our own crypto would disqualify a security-first project. Any experimental scheme uses a *published, peer-reviewed* construction and is clearly labelled. Decisions: [ADR-0010](../adr/0010-crypto-envelope-aead.md), [ADR-0012](../adr/0012-client-side-encryption.md).

> **Implementation status (Phase 1).** Encryption-at-rest is shipped and on by default. The `quiver-crypto` crate provides an `AeadCodec` (XChaCha20-Poly1305 with per-page/per-record HKDF-SHA256 subkeys and a fresh random 192-bit nonce per seal, from the RustCrypto crates — no `ring`, no home-grown code). It is wired into the storage engine through the `PageCodec` seam so it seals **all** durable data: the paged manifest and segment files *and* the record-framed write-ahead log (the WAL is sealed per record, since a page-only codec would otherwise leave it in plaintext). Phase 1 uses a single operator-supplied 256-bit root key (`QUIVER_ENCRYPTION_KEY`); the full envelope hierarchy below (master key → per-collection DEK → KMS, and crypto-shredding) is Phase 3 "security depth". TLS-in-transit is shipped too: `rustls` over the audited `ring` provider (no OpenSSL, no `aws-lc-rs` C toolchain) terminates TLS for REST (via `axum-server`) and gRPC (via tonic's `tls-ring`), and a non-loopback bind requires it.

## Key hierarchy (envelope encryption)

```text
Master Key (MK)            ── from env file (0600) or external KMS; never on disk in plaintext
  └─ wraps ──> Collection DEK (256-bit, random, per collection)
                 └─ derives ──> per-page subkey = HKDF-SHA256(DEK, info = file_id ‖ page_id ‖ page_version)
                                   └─ AEAD-seals each 16 KiB page
```

- **MK** is supplied by the operator via a file (mode `0600`) or an external **KMS** (the server calls KMS to wrap/unwrap DEKs). The MK never touches disk in plaintext.
- **DEKs** are random 256-bit keys, one per collection, stored **wrapped** by the MK in the collection metadata (wrap via AES-256-GCM-SIV / AES-KW, or KMS Encrypt). Plaintext DEKs live only in RAM and are **zeroized** (`zeroize`) on drop.
- **Per-page subkeys** are derived with HKDF-SHA-256 from the DEK and a unique context, so **nonce reuse is impossible by construction** (each page-version is sealed under a unique key) — this side-steps AES-GCM's catastrophic nonce-reuse failure mode without relying on a global nonce counter.

## AEAD selection

Both options are standard, audited AEADs; the choice is recorded in the collection key metadata so data stays decryptable if the default changes:

- **AES-256-GCM** — default when **AES-NI** (hardware AES) is detected: fastest there, and the expected choice in compliance contexts.
- **ChaCha20-Poly1305** — default when AES-NI is absent: constant-time in software, no timing-side-channel dependence on hardware AES. (`XChaCha20-Poly1305`'s extended nonce is available where random nonces are preferable.)

The selection is automatic by default and overridable by config/compliance policy. **AES-256-GCM-SIV** (nonce-misuse-resistant) is used for DEK wrapping.

## In transit

**TLS 1.3 via `rustls`** (a memory-safe, audited stack — no OpenSSL). Non-loopback binds **require** TLS (the server refuses to serve plaintext on a public interface absent an explicit, warned opt-out). **mTLS** is optional: client identity = certificate subject, mapped to an RBAC principal.

## Secrets handling

- Secrets (MK, KMS creds, TLS keys) come from env/KMS/files with strict modes — **never** committed, **never** logged, **never** in the config file in plaintext (the config references a secret *source*).
- `gitleaks` runs pre-commit and in CI; `.env.example` documents every variable; key material in memory is wrapped in `zeroize`-ing types.

## Crypto-shredding

Because each collection has its own DEK, destroying that wrapped DEK renders the collection's at-rest data (and backups) cryptographically unrecoverable — instant, verifiable erasure.

## Client-side payload encryption (ADR-0012)

A client may encrypt payloads with a key Quiver never sees; the server stores and returns the ciphertext as an opaque blob and performs no server-side filtering on those fields. This protects payload confidentiality against the server/operator (adversary A4). **It does not encrypt vectors** — see the threat model's honest boundary statement.

## Experimental: vector confidentiality vs the server (DCPE)

Standard ANN needs plaintext vectors server-side. The only path to *vector* confidentiality against the server is **property-preserving encryption** — specifically a **published distance-comparison-preserving encryption (DCPE)** scheme — implemented **behind an experimental feature flag**, off by default. We will:

- implement a **specific, cited, peer-reviewed** construction (named precisely when the feature is built), **never** an invented one;
- **document the leakage honestly**: DCPE reveals approximate distances/ordering by design (that is what lets the server rank), which is a real confidentiality reduction and is *not* equivalent to semantic security;
- make **no claim** of homomorphic-encrypted search in core.

## Test posture

Known-answer/test vectors for every AEAD and KDF; a test proving on-disk files are ciphertext; a test proving a client-side-encrypted payload is unreadable server-side; fuzzing of the parsers; `cargo audit`/`deny` on the dependency set.
