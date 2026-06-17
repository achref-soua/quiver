# Encrypted vector search (DCPE) — experimental

Quiver can search your embeddings on a server that **never sees the plaintext
vectors or the key**, using **Distance-Comparison-Preserving Encryption (DCPE)**.
This page is the honest specification of what that does and — just as important —
what it does **not** do. Read it before turning the feature on.

> [!WARNING]
> **DCPE is experimental, is _not_ semantically secure, and leaks information by
> design.** It is off by default. It is a *different, weaker* tool than
> encryption-at-rest ([ADR-0010](../adr/0010-crypto-envelope-aead.md)) or
> client-side payload encryption ([ADR-0012](../adr/0012-client-side-encryption.md)),
> for a *different* problem: approximate nearest-neighbour search over encrypted
> vectors on an untrusted server. It **complements** encryption-at-rest; it does
> not replace it. For a **semantically secure** alternative that leaks *nothing*
> about the vectors — at the cost that the server no longer ranks — see
> [client-side opaque vectors](client-side-vectors.md).

## The problem it solves

To run an ordinary ANN search, a server needs the **vectors in plaintext** — and
embeddings can be inverted to approximately reconstruct the source text or image.
Encryption-at-rest protects a stolen disk but still exposes plaintext vectors to
the running server. DCPE closes exactly that gap: the client encrypts vectors
*before* upload, the server stores and indexes the ciphertexts, and search still
works because the **ordering of Euclidean distances is preserved** — without the
server ever holding the key or the plaintext.

## The scheme

Quiver implements **Scale-And-Perturb (SAP)**, the published construction of
Fuchsbauer, Ghosal, Hauke & O'Neill, *"Approximate Distance-Comparison-Preserving
Symmetric Encryption"* (IACR ePrint [2021/1666](https://eprint.iacr.org/2021/1666),
SCN 2022) — the same scheme behind IronCore Labs' Cloaked AI. **No primitive is
invented**; only the published composition, built from audited RustCrypto crates
(ChaCha20, HMAC-SHA256, HKDF-SHA256).

This is **cipher v2** ([ADR-0035](../adr/0035-docs-site-and-dcpe-hardening.md)): it
adds the paper's two hardening steps — a key-derived component **shuffle** and an
optional ordering-preserving **normalisation** — on top of the core SAP cipher of
[ADR-0031](../adr/0031-dcpe-vector-encryption.md). v2 is a breaking change from v1
(v1 ciphertexts are not v2-decryptable); since the cipher is client-side, there is
**no on-disk format change**.

One master secret derives, via HKDF-SHA256, a secret scaling factor `s ∈ [1, 2)`,
a CSPRNG key, a shuffle CSPRNG key, and an HMAC key. To encrypt `m ∈ ℝ^d` with
approximation factor `β ≥ 0`:

1. **normalise** (optional, ordering-preserving): apply a *fixed* global affine
   transform `m₁ = (m − μ)·α` — a per-dimension shift vector `μ` (default `0`) and a
   **single** positive scalar `α` (default `1`);
2. **shuffle**: permute the components with a permutation `π` derived **from the key
   alone** (HKDF sub-key + a fixed-IV ChaCha20 Fisher–Yates), identical for every
   vector and query;
3. draw a fresh random 96-bit IV;
4. seed ChaCha20 from `(prfKey, iv)` and sample a perturbation `λ` **uniformly in
   the d-ball of radius `(s/4)·β`** (a Box-Muller Gaussian direction normalised
   and scaled by `radius = (s/4)·β·U^{1/d}`, `U ~ Uniform[0,1)`);
5. the ciphertext vector is `c = s·π(m₁) + λ`;
6. an HMAC-SHA256 tag over `(domain ‖ β ‖ iv ‖ c)`, with the domain bumped to
   `quiver/dcpe/v2/tag`, gives tamper-evidence (a v1 ciphertext fails a v2 integrity
   check — fail-closed — rather than decrypting to garbage).

Decryption re-derives `λ` from `(prfKey, iv)` (the perturbation is pseudorandom,
so it cancels), verifies the tag, and reverses the pipeline `m = T⁻¹(π⁻¹((c − λ)/s))`.
A query is encrypted the same way: the secret `s`, the permutation `π`, and the
normalisation are identical for data and queries, so they cancel in the distance
ordering, while the bounded per-vector perturbations are the margin `β`.

Both hardening steps **preserve the L2 distance-comparison ordering exactly**: L2
distance is invariant under any permutation of coordinates (so the shuffle costs
zero recall and only hides which ciphertext coordinate is which plaintext
coordinate), and a uniform per-coordinate shift cancels in any difference while a
single positive scalar scales every distance by the same factor (so normalisation
just canonicalises the cloud's centroid and overall scale — making `β`'s meaning
consistent across datasets). The shuffle and normalisation therefore harden the
cipher **without changing its accuracy or its leakage class.**

## What it hides, and what it leaks — honestly

Against an **honest-but-curious server** that sees only ciphertexts, with **no
known plaintext/ciphertext pairs** and **no strong prior** on the embedding
distribution, DCPE hides the exact coordinate values, the exact pairwise distances
(perturbed by up to the ball radius), the coordinate frame (via the secret scale),
and equality of repeated vectors (randomised IVs).

**It leaks, by design:** the **approximate Euclidean distance-comparison relation**
among the ciphertexts — hence approximate pairwise distances (up to the secret
scale and the margin), cluster structure, and dataset geometry. That leakage is
*the mechanism that makes encrypted search work*: anyone holding the ciphertexts
can run the same nearest-neighbour search and clustering you can.

**It is broken by** an adversary with **known plaintext/ciphertext pairs** (the
low-entropy secret scale becomes recoverable), or with a **strong distributional
prior** on the embeddings or access to the **embedding model** (embedding-inversion
attacks apply — preserving distance preserves much of what inversion needs). DCPE
assumes a high-entropy message distribution; real embeddings may not meet that.

It is **not IND-CPA**, and Quiver never claims it is. There is no homomorphic
search in core, and no home-grown scheme.

## The accuracy/security trade-off

The approximation factor `β` is the knob. A larger `β` adds more perturbation —
hiding exact distances better but lowering search recall; a smaller `β` keeps
recall high but hides less. Quiver's tests demonstrate this directly: recall stays
high at a small `β` and degrades as `β` grows. Tune `β` against your own data and
recall target; there is no universally correct value.

## Constraints

- **L2 only.** The secret scaling changes vector norms, so cosine and inner-product
  orderings are not preserved. A DCPE collection must use the `l2` metric (the
  server rejects anything else with a 400).
- **Encrypt and query from the same client**, with the same key, `β`, and
  normalisation.
- **Normalisation cannot whiten per-axis variance.** The optional normalisation is a
  *global* affine transform (a per-axis shift plus a single scalar scale) because
  that is the strongest normalisation that preserves the L2 distance-comparison
  ordering. Standardising each dimension by its own variance is *anisotropic* — it
  re-weights the dimensions in the L2 distance and so **breaks the ordering** the
  untrusted server ranks on. Full per-axis whitening is therefore **incompatible**
  with searchable DCPE and is deliberately not offered; supply real corpus
  statistics (a per-axis mean as the shift, a global RMS radius as `1/scale`) to get
  the normalisation that *is* compatible.
- The float-valued ciphertext uses transcendental functions, so cross-language
  reproduction is validated within a tolerance, not bit-exactly (the integrity tag,
  being over bytes, *is* bit-exact). The Rust module `quiver_crypto::dcpe` is the
  canonical reference.

## Using it

Create the collection with the flag, encrypt vectors and queries client-side:

```python
from quiver import Client
from quiver.dcpe import DcpeCipher          # pip install quiver-client[dcpe]

cipher = DcpeCipher.from_hex("…64 hex chars…", approximation_factor=0.02)
with Client("https://…", api_key="…") as q:
    q.create_collection("vault", dim=8, metric="l2", vector_encryption="dcpe")
    sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8])
    q.upsert("vault", [{"id": "a", "vector": sealed.ciphertext}])
    hits = q.search("vault", cipher.encrypt_query(my_query), k=10)
```

A native cipher ships in all three languages — the Rust reference
(`quiver_crypto::dcpe::DcpeCipher`, available to embedders), the Python
`quiver.dcpe.DcpeCipher`, and the TypeScript `DcpeCipher` at the
`quiver-client/dcpe` subpath (`pnpm add @stablelib/{chacha,hkdf,hmac,sha256}`):

```ts
import { DcpeCipher } from "quiver-client/dcpe";

const cipher = DcpeCipher.fromHex("…64 hex chars…", 0.02);
const sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
// upsert sealed.ciphertext; search with cipher.encryptQuery(myQuery).
```

To use the optional normalisation, build it from a one-time measurement of your
corpus and pass it to the cipher (`Normalization` in each SDK; `with_normalization`
on the Rust cipher). The MCP `create_collection` tool accepts
`vector_encryption="dcpe"`.

## Key management

The DCPE key is the client's; Quiver never sees it. **Use a dedicated key** —
never your at-rest (`QUIVER_ENCRYPTION_KEY`) or payload key. Losing the key makes
the vectors unrecoverable. Ideally use a distinct key per collection.

## Status and follow-ups

Shipped in v0.10.0 ([ADR-0031](../adr/0031-dcpe-vector-encryption.md)): the core
scale-and-perturb cipher with an integrity tag, the per-collection flag across the
API/MCP/SDKs, the Python cipher, and an end-to-end gate proof.

**Hardened to cipher v2 in v0.15.0**
([ADR-0035](../adr/0035-docs-site-and-dcpe-hardening.md)): the paper's two
security-boosting pre-processing steps now ship — the secret component **shuffle**
(a key-derived permutation, an exact L2 isometry) and an ordering-preserving global
**normalisation** (a per-dimension shift plus a single scalar scale) — together with
a native **TypeScript** cipher, closing the SDK gap so Rust, Python, and TypeScript
all have native DCPE ciphers validated against each other by a cross-language
known-answer test. The honest limit recorded above stands: full per-axis variance
whitening is incompatible with searchable encryption and is not offered. v2 is a
breaking cipher change (v1 ciphertexts are not v2-decryptable), acceptable because
DCPE is experimental, off by default, and stored as ordinary L2 vectors (no on-disk
change).
