# Cryptography

**Non-negotiable: Quiver implements no cryptographic primitives or protocols of its own.** Every primitive comes from an audited library — `rustls` for TLS, and RustCrypto crates for AEAD, hashing, KDF, and key wrapping. Rolling our own crypto would disqualify a security-first project. Any experimental scheme uses a *published, peer-reviewed* construction and is clearly labelled. Decisions: [ADR-0010](../adr/0010-crypto-envelope-aead.md), [ADR-0012](../adr/0012-client-side-encryption.md).

> **Implementation status (Phase 1).** Encryption-at-rest is shipped and on by default. The `quiver-crypto` crate provides an `AeadCodec` (XChaCha20-Poly1305 with per-page/per-record HKDF-SHA256 subkeys and a fresh random 192-bit nonce per seal, from the RustCrypto crates — no `ring`, no home-grown code). It is wired into the storage engine through the `PageCodec` seam so it seals **all** durable data: the paged manifest and segment files *and* the record-framed write-ahead log (the WAL is sealed per record, since a page-only codec would otherwise leave it in plaintext). TLS-in-transit is shipped too: `rustls` over the audited `ring` provider (no OpenSSL, no `aws-lc-rs` C toolchain) terminates TLS for REST (via `axum-server`) and gRPC (via tonic's `tls-ring`), and a non-loopback bind requires it.
>
> **Update (Phase 3).** The **envelope hierarchy below is now shipped.** `quiver-crypto`'s `EnvelopeKeyRing` makes `QUIVER_ENCRYPTION_KEY` a **master key** that wraps a random per-collection **DEK** (stored wrapped under `<data_dir>/keys/<id>.dek`); each collection's segments and index are sealed under its own DEK, and the catalog (manifest + WAL) under a master-key-derived catalog key. This makes **crypto-shredding** real (below). Sourcing the master key from a `0600` file or a KMS is the remaining slice; the AEAD throughout is XChaCha20-Poly1305 (AES-256-GCM auto-select remains a future option). **Format note:** this changes the at-rest key hierarchy from v0.2.0's single root key — pre-1.0 there is no migrator, so re-create encrypted collections under v0.3.0.

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
- **Master key source (shipped):** the MK is `QUIVER_ENCRYPTION_KEY` (hex) **or** `QUIVER_MASTER_KEY_FILE` (a `0600` file holding the hex), exactly one of the two. The file form suits a mounted Docker/Kubernetes secret or a KMS-decrypted file; a group/world-readable key file is warned about at startup. A built-in KMS client is a future decrypt-to-file step in front of this. The MK never touches disk via Quiver, and plaintext DEKs in memory are wrapped in `zeroize`-ing types.
- `gitleaks` runs pre-commit and in CI; `.env.example` documents every variable; key material in memory is wrapped in `zeroize`-ing types.

## Crypto-shredding

Because each collection has its own DEK, destroying that wrapped DEK renders the collection's at-rest data cryptographically unrecoverable — even to the master-key holder, and even if the ciphertext survives in a backup. This is instant, verifiable erasure without overwriting every byte (the GDPR "right to erasure" pattern).

`Store::shred_collection` / `Database::shred_collection` drops the collection, checkpoints (so any un-checkpointed rows are sealed into DEK-protected segments and the catalog-keyed WAL is rotated away), then deletes `<data_dir>/keys/<id>.dek`. A plain `drop_collection` also reclaims the DEK at the next checkpoint's garbage collection. After a shred, opening the collection's codec fails — the DEK is gone — so its segments and index are permanently undecryptable (`quiver-crypto/tests/envelope_shred.rs` proves this end-to-end).

**Scope:** erasure covers the durable **segments and index** (the bulk store). A WAL *backup* captured before the shred would still be master-key-decryptable until rotation — the inherent caveat of erasing data that was already copied elsewhere.

## Client-side payload encryption (ADR-0012)

A client may encrypt payloads with a key Quiver never sees; the server stores and returns the ciphertext as an opaque blob and performs no server-side filtering on those fields. This protects payload confidentiality against the server/operator (adversary A4). **It does not encrypt vectors** — see the threat model's honest boundary statement.

### Envelope format (the cross-language contract)

The reference implementation is [`quiver_crypto::payload::PayloadCipher`](https://github.com/achref-soua/quiver/blob/main/crates/quiver-crypto/src/payload.rs); the Python and TypeScript SDKs mirror it byte-for-byte. A sealed value is one JSON object with a single reserved key:

```json
{ "__quiver_enc__": {
    "v": 1,
    "alg": "xchacha20poly1305",
    "n":  "<base64 24-byte nonce>",
    "ct": "<base64 ciphertext + 16-byte Poly1305 tag>"
} }
```

- **AEAD:** XChaCha20-Poly1305 (the same audited RustCrypto primitive as at-rest), a fresh random 192-bit nonce per seal — nonce reuse is impossible by construction. The associated data `quiver/payload/v1` binds every ciphertext to this format version.
- **Key:** a dedicated 256-bit key, used directly (no derivation) so the envelope is reproducible in any language. The plaintext is the UTF-8 JSON serialization of the original value.

### Keeping some fields filterable

Encrypted fields cannot be filtered or indexed server-side. To keep a field server-filterable, leave it in cleartext and merge the sealed envelope alongside it — `open` reads only the reserved key and ignores cleartext siblings:

```jsonc
// stored payload: `tier` stays filterable; `ssn` is opaque to the server
{ "tier": "gold", "__quiver_enc__": { "v": 1, "alg": "xchacha20poly1305", "n": "…", "ct": "…" } }
```

### Key management & honest limits

The client owns the key. **Never reuse the `QUIVER_ENCRYPTION_KEY` (at-rest) key** for payloads, and never send the payload key to the server. Losing the key means the data is unrecoverable. The boundary is exact: this hides *only* the sealed fields; cleartext siblings and all vectors remain visible to the server.

## Vector confidentiality vs the server

Standard ANN needs plaintext vectors server-side, so vector confidentiality against the server is **opt-in, per collection** (`vector_encryption`), at two honest points on a spectrum — both client-side, the server never holding the key.

**DCPE (`dcpe`, experimental, [`dcpe.md`](./dcpe.md)).** A *published, peer-reviewed* distance-comparison-preserving construction (Scale-And-Perturb — never invented), so the server keeps ranking ciphertexts by approximate L2 distance. It **reveals approximate distances/ordering by design** (that is what lets the server rank) — a real confidentiality reduction, **not** semantic security.

**Client-side opaque vectors (`client_side`, semantically secure, [`client-side-vectors.md`](./client-side-vectors.md)).** [`quiver_crypto::vector::VectorCipher`](https://github.com/achref-soua/quiver/blob/main/crates/quiver-crypto/src/vector.rs) seals the vector's raw little-endian `f32` bytes with the **same** XChaCha20-Poly1305 envelope as payloads (no new primitive), under the reserved key `__quiver_vec__` with associated data `quiver/vector/v1`:

```json
{ "__quiver_vec__": { "v": 1, "alg": "xchacha20poly1305", "dim": 8, "n": "…", "ct": "…" } }
```

The server stores the blob plus a zero placeholder vector and does **no** distance math, so it is genuinely **IND-CPA** for vectors — at the cost that the server cannot rank (the client fetches and ranks). The Python and TypeScript SDKs mirror the envelope **bit-exactly** (raw bytes, no transcendental floats).

Core makes **no claim** of homomorphic-encrypted search.

## Test posture

Known-answer/test vectors for every AEAD and KDF; a test proving on-disk files are ciphertext; a test proving a client-side-encrypted payload is unreadable server-side; fuzzing of the parsers; `cargo audit`/`deny` on the dependency set.
