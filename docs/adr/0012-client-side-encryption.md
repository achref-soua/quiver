# ADR-0012: Client-side encryption & trust boundary

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Some users must keep payloads confidential **even from the server/operator** (threat-model adversary A4). We need a clear, honest mechanism and an equally honest statement of its limits. See [`../security/threat-model.md`](../security/threat-model.md) and [`../security/crypto.md`](../security/crypto.md).

## Decision

- **Client-side payload encryption:** clients may encrypt payloads with a key Quiver never sees; the server stores and returns the ciphertext as an **opaque blob** and performs **no server-side filtering** on encrypted fields. The SDKs provide helpers (AEAD with a client-held key) but the key and plaintext never leave the client.
- **Honest boundary:** this protects **payloads, not vectors**. Standard ANN requires plaintext vectors server-side, which can leak information about their source. We document this explicitly and do **not** claim the server "sees nothing."
- **Vector confidentiality (experimental only):** addressed solely by an **experimental** feature flag implementing a **published** distance-comparison-preserving encryption (DCPE) scheme, with documented leakage caveats (approximate distance/order is revealed by design). **No invented schemes; no homomorphic-search claim in core.**

## Consequences

- **+** A real zero-server-knowledge option for payloads; the trust boundary moves to the client for that data; honest scoping preserves credibility.
- **−** Encrypted payload fields cannot be filtered/indexed server-side (filtering must use non-encrypted fields or happen client-side); the client owns key management (loss = data loss — documented).

## Alternatives considered

- **Claiming end-to-end encryption of everything** — rejected as dishonest: plaintext vectors are required for default ANN.
- **Building homomorphic / fully-oblivious search into core** — rejected for v1: impractical performance and easy to overclaim; only a cited DCPE scheme behind an experimental flag.
- **Server-side-only encryption (no client option)** — insufficient for users who don't trust the operator.
