# Encrypted vector search

Search your embeddings on a server you don't fully trust, choosing **per
collection** (`vector_encryption`) where you sit on the confidentiality/performance
spectrum — because no scheme gives fast server-side ranking, zero leakage, and
practical performance all at once.

Both modes are opt-in and **off by default**, and they **complement** encryption at
rest rather than replacing it.

## DCPE (`vector_encryption: "dcpe"`, experimental)

The client encrypts vectors with **distance-comparison-preserving encryption** —
the published [Scale-And-Perturb scheme](https://eprint.iacr.org/2021/1666), built
only from audited RustCrypto primitives — so the server can rank ciphertexts by
approximate L2 distance **without ever holding the plaintext vectors or the key**.

It is **not semantically secure**: L2-only, and it **leaks the approximate
distance-comparison relation by design** (that is how the server ranks). It carries
real, documented caveats and is broken by known-plaintext or strong-prior
adversaries. Read the full specification — including the v2 hardening (a key-derived
component shuffle and an ordering-preserving global normalisation) and what it can
and cannot do — on the [DCPE page](../security/dcpe.md) before using it.

Native ciphers ship in **Rust, Python, and TypeScript**, validated against each
other by a cross-language known-answer test.

## Client-side opaque vectors (`vector_encryption: "client_side"`, semantically secure)

The server stores only XChaCha20-Poly1305 ciphertext (the same audited AEAD as
at-rest — no new cryptography) plus a zero placeholder, does **no** distance math,
and learns **nothing** about the vectors — no coordinates, no distances, no geometry
(genuinely IND-CPA).

The honest cost: the server doesn't rank, so the client fetches the (optionally
pre-filtered) set and ranks locally — best for small/medium or server-pre-filtered
collections. Native `VectorCipher`s ship in Rust/Python/TypeScript with a bit-exact
cross-language test, plus a `search`-style helper that hides the fetch-and-rank
round-trip. Read the [client-side opaque vectors page](../security/client-side-vectors.md).

## Which one?

| | DCPE | Client-side opaque |
|---|---|---|
| Server ranks? | yes (approximate L2) | no (client ranks) |
| Semantically secure? | no (leaks distance ordering) | yes (IND-CPA) |
| Metric | L2 only | any (client-side) |
| Best for | server-side ANN with a weaker, honest guarantee | strong confidentiality, small/pre-filtered sets |

See the [cryptography overview](../security/overview.md) for how these fit Quiver's
broader posture, and the [threat model](../security/threat-model.md) for the
boundaries.
