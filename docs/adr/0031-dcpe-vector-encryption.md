# ADR-0031: Experimental property-preserving vector encryption (DCPE)

- **Status:** Proposed
- **Date:** 2026-06-15
- **Deciders:** Achref Soua

## Context

Quiver already protects data three ways: **encryption-at-rest** seals every
durable page and WAL record (ADR-0010), so someone who steals the disk learns
nothing; **client-side payload encryption** (ADR-0012) lets a caller seal JSON
payloads the server stores as opaque blobs; and **RBAC + TLS + audit** (ADR-0011)
guard the network surface. All three share one gap: to run an ordinary
nearest-neighbour search, the server needs the **vectors themselves in
plaintext**. The embeddings — which can be inverted to approximately reconstruct
the source text or image — are exposed to whoever runs the server.

The build brief lists, as an explicitly **experimental / advanced** item, an
opt-in **property-preserving encryption (PPE)** mode for vectors that closes this
gap: encrypt the embeddings so the server can still answer approximate
nearest-neighbour queries **without ever seeing the plaintext vectors**. This is
a real, published primitive — **Distance-Comparison-Preserving Encryption
(DCPE)** — and a real product category (IronCore Labs' *Cloaked AI*). It is also
**weaker than semantic security by design**, and the brief is emphatic: ship a
*published* scheme, with *honest* caveats, default **off**, and **never** present
it as semantically secure or invent our own crypto.

This ADR decides whether and how to add that mode. It is design-first: no crypto
code lands before this record is reviewed.

### What DCPE is, precisely

A symmetric encryption scheme is **β-distance-comparison-preserving** if, for
encryptions of vectors, the *ordering* of pairwise (Euclidean) distances is
preserved **up to an approximation margin β**: if `dist(x, y)` is enough smaller
than `dist(x, z)`, then with high probability `dist(Enc(x), Enc(y)) <
dist(Enc(x), Enc(z))`. That is exactly the property an approximate-nearest-
neighbour index relies on, so ANN search runs on ciphertexts unchanged.

The scheme we adopt is **Scale-And-Perturb (SAP)** from Fuchsbauer, Ghosal,
Hauke & O'Neill, *"Approximate Distance-Comparison-Preserving Symmetric
Encryption"* (IACR ePrint 2021/1666; SCN 2022), the construction underlying
IronCore's Cloaked AI. The idea, verbatim from the paper: **scale** every vector
by a secret factor, then **perturb** it by adding a bounded pseudorandom vector.
Concretely, with secret scaling factor `s > 0`, a per-message random IV, and an
approximation factor `β ≥ 0`, to encrypt `m ∈ ℝ^d`:

```text
seed a CSPRNG from (key, iv)
u  ← N(0, I_d)                       # d i.i.d. standard normals (a direction)
x' ← U(0, 1)                         # one uniform
r  ← (s/4) · β · x'^(1/d)            # radius: uniform in a d-ball of radius (s/4)·β
λ  ← r · u / ‖u‖                     # the perturbation (uniform point in that ball)
c  ← s · m + λ                       # the ciphertext vector
tag ← HMAC-SHA256(authKey, iv ‖ β ‖ c)   # integrity / tamper-evidence
```

Decryption re-derives `λ` from `(key, iv)` (the perturbation is **pseudorandom,
not truly random**, so it cancels), verifies the tag, and returns
`m ← (c − λ) / s`. Encryption is randomised (a fresh IV per call ⇒ the same
vector encrypts differently each time, hiding equality), yet exactly invertible
by the key holder. Querying encrypts the query vector the same way; the secret
scale `s` is identical for data and queries, so it cancels in distance ordering,
while the per-vector perturbations stay bounded by the ball radius — which is the
β-DCP margin.

The paper also defines two **security-boosting pre-processing** steps: (1)
**normalising the plaintext distribution** (a data-dependent calibration the
client applies before encryption) and (2) **shuffling** — a secret, key-derived
permutation of the vector components. A permutation is an isometry, so it
preserves Euclidean distance (zero recall cost) while hiding which ciphertext
coordinate is which plaintext coordinate. We adopt shuffling as part of the
scheme and document distribution-normalisation as recommended client-side
calibration.

### Honest threat model — what it hides and what it leaks

This is the part that must not be soft-pedalled.

**DCPE is _not_ IND-CPA secure, and we will never claim it is.** It deliberately
leaks a relation over ciphertexts so that search can work. Precisely:

- **What it hides** (against an honest-but-curious server that sees only
  ciphertexts and has no known plaintext/ciphertext pairs and no strong prior on
  the embedding distribution): the exact coordinate values, the exact pairwise
  distances (perturbed by ≤ the ball radius), the absolute coordinate frame (via
  the secret scale and the component shuffle), and equality of repeated vectors
  (randomised IVs).
- **What it leaks _by design_:** the **approximate Euclidean-distance comparison
  relation** among the encrypted vectors — hence approximate pairwise distances
  (up to the secret scale and the β margin), cluster structure, and the
  approximate geometry of the dataset. Anyone holding the ciphertexts can run the
  same ANN/clustering the legitimate user can; that is the whole point.
- **What breaks it:** an adversary with **known plaintext/ciphertext pairs** can
  recover the low-entropy secrets (the single scalar `s`, the permutation) and
  then approximately invert; an adversary with a **strong distributional prior**
  on the embeddings, or access to the **embedding model**, can mount approximate
  reconstruction attacks (embedding-inversion research applies — preserving
  distances preserves much of what inversion needs). DCPE assumes a
  high-min-entropy message distribution; real embeddings may not meet that.

So the security notion is the paper's weaker, formally-stated one (security only
up to the approximate distance-comparison leakage, for high-entropy message
distributions), **not** the IND-CPA guarantee that encryption-at-rest (ADR-0010)
and payload encryption (ADR-0012) provide. DCPE is a **distinct, weaker tool for
a distinct problem** (search over encrypted vectors on an untrusted server), and
it composes *with* at-rest encryption — it does not replace it.

The metric matters: SAP is built for **Euclidean (L2)** distance. The secret
scaling changes vector norms, so cosine and inner-product orderings are **not**
preserved. A DCPE collection is therefore L2-only.

## Decision

Add DCPE as an **opt-in, experimental, client-side** vector-encryption mode,
clearly labelled and **off by default**.

**1. The scheme lives in `quiver-crypto`, built only from audited primitives.**
A new `quiver_crypto::dcpe` module implements SAP faithfully: ChaCha20 (RustCrypto
`chacha20`) as the deterministic CSPRNG seeded from `(key, iv)`; standard normals
via the Box-Muller transform over that keystream; the d-ball radius exactly as
above; HMAC-SHA256 (RustCrypto `hmac` + `sha2`) for the integrity tag; HKDF-SHA256
(the crate's existing dependency) to derive the scale `s`, the PRF key, the auth
key and the shuffle permutation from one master secret. We implement no
cryptographic primitive of our own — only the published *composition*. The exact
byte-level sampling (keystream layout, the u64→f64 mapping, Box-Muller) is
specified so it is reproducible in another language.

**2. Encryption is client-side; the server stores ciphertext vectors and is
oblivious to the key.** `DcpeCipher::encrypt` returns the ciphertext vector
(upserted like any vector and indexed normally) plus an IV and auth tag the
caller stows (e.g. in payload) if it wants to decrypt later. `encrypt_query`
encrypts a query vector for search. The server runs ordinary L2 ANN over the
ciphertexts and never holds the DCPE key — the strongest honest form of the
guarantee.

**3. An opt-in per-collection flag marks and constrains the mode.** A new
`Descriptor.encrypted_vectors: bool` (serde-default `false`, with the same
decode-fallback chain that kept older descriptors readable) records that a
collection holds DCPE ciphertexts. When set, collection creation **requires the
L2 metric** and rejects Cosine/Dot. The flag is surfaced over REST, gRPC, the MCP
tool, and the SDKs so the mode is discoverable, auditable, and self-documenting —
and so the honest "experimental, leaks distance comparisons" caveat travels with
it.

**4. Honest by construction, everywhere.** The Rustdoc, the dedicated design page
(`docs/security/dcpe.md`), the threat model, the README and the API descriptions
all state plainly that DCPE is experimental, is **not** semantically secure,
leaks the approximate distance-comparison relation by design, is broken by
known-plaintext or strong-prior adversaries, and must use a **dedicated** key
(never the at-rest key). It is presented as a tool for a specific threat model,
with its limits, not as a general confidentiality control.

**5. Key management is the client's, and documented.** One master secret derives
all DCPE sub-keys; the client owns it and Quiver never sees it. Use a dedicated
key per deployment (ideally per collection); losing it makes the vectors
unrecoverable; reusing the at-rest key is forbidden.

## Consequences

- **+** Quiver gains a genuine, published answer to "search my embeddings on a
  server I don't fully trust" — the headline differentiator the brief calls for,
  shipped open-source.
- **+** Reuses the existing seams: a client-side cipher (like ADR-0012), an
  opt-in descriptor flag (like the `multivector` flag, ADR-0028), and the normal
  index path. **No on-disk format change and no change to the `kill -9` crash
  gate** — encrypted vectors are just vectors to the engine.
- **+** Composes with encryption-at-rest and RBAC; orthogonal to them.
- **−** It is **weaker than IND-CPA and leaks by design.** This is an inherent
  property of the primitive, mitigated only by relentless honesty in the docs and
  API. Mis-sold, it is a footgun; that risk is real and is why the mode is
  experimental and off by default.
- **−** Accuracy/security trade-off: a larger β hides exact distances better but
  lowers recall. The client must tune β; we document the trade-off and verify it.
- **−** L2-only; cosine/dot collections cannot use DCPE.
- **−** The ciphertext is float-valued and its computation uses transcendental
  functions (`ln`, `cos`, `sqrt`, `pow`), so **bit-exact cross-language
  reproduction is not guaranteed** (libm ULP differences). Interop across SDK
  languages is validated within a tolerance; the recommended pattern is to
  encrypt and query from the same client. The Rust module is the canonical
  reference.

## Verification

- **Round-trip:** `decrypt(encrypt(m)) ≈ m` within float tolerance; the auth tag
  rejects a tampered ciphertext and a wrong key.
- **Distance comparison is preserved:** over random datasets, top-k ANN recall on
  DCPE ciphertexts versus plaintext stays high at a small β and **degrades as β
  grows** — demonstrating the security/accuracy trade-off as a test, not a claim.
- **The server never sees plaintext (gate proof, mirroring ADR-0012):** boot a
  server with encryption-at-rest **off** so DCPE is the only thing hiding the
  vectors; DCPE-encrypt and upsert; an encrypted query returns the right
  neighbours; the plaintext coordinate values are **absent** from the on-disk
  files.
- **The leak is real (honest positive control):** a test shows that approximate
  pairwise distances *are* recoverable from ciphertexts alone — proving we are not
  overclaiming secrecy.
- **Cross-language:** each SDK port round-trips and preserves distance within its
  own language; a tolerance-based known-answer test checks it reproduces the Rust
  reference ciphertext closely (catching algorithm divergence, allowing ULP
  drift).

## Alternatives considered

- **Fully homomorphic encryption (FHE) / secure multi-party computation** —
  rejected for the 0.x line: the only options that are *not* leaky, but orders of
  magnitude too slow and complex for interactive ANN at scale, and far beyond the
  single-node wedge.
- **Order-preserving / order-revealing encryption (OPE/ORE)** — rejected: built
  for scalar order, not high-dimensional Euclidean distance; applied per
  coordinate it leaks more and does not preserve vector distances.
- **Inventing our own distance-preserving transform** — rejected outright. The
  brief forbids it, and home-grown crypto is exactly how this category gets
  dangerous. We implement a peer-reviewed scheme from audited primitives and cite
  it.
- **Server-side DCPE (the server holds the key and encrypts on ingest)** —
  rejected: it would expose plaintext vectors to the server, defeating the entire
  purpose. DCPE is only meaningful client-side.
- **Doing nothing / "use at-rest encryption"** — insufficient: at-rest encryption
  protects a stolen disk but exposes plaintext vectors to the running server,
  which is precisely the threat DCPE addresses. They are complementary, not
  substitutes.
