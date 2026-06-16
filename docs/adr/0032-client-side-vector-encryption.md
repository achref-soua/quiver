# ADR-0032: Semantically secure client-side vector encryption (opaque vectors)

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Achref Soua

## Context

DCPE (ADR-0031) gave Quiver a real, published way to search embeddings on an
untrusted server — but at a price that ADR stated plainly: DCPE is **not**
semantically secure. It leaks the approximate distance-comparison relation among
ciphertexts **by design**, because that leak is exactly what lets the server rank
them. For some users that is a dealbreaker: they need the server to learn
**nothing** about their vectors beyond coarse access patterns — no coordinates,
no distances, no clustering, no geometry. That is the **IND-CPA / semantic
security** guarantee that encryption-at-rest (ADR-0010) and payload encryption
(ADR-0012) already provide for pages and payloads, and that DCPE deliberately
does not provide for vectors.

This ADR adds a second, complementary vector-encryption mode that **is**
semantically secure. It is design-first: no code lands before this record is
reviewed.

### The tradeoff is fundamental — name it, do not paper over it

You cannot have all three of:

1. an **untrusted server doing fast ANN ranking**,
2. **semantic security / zero distance leakage**, and
3. **practical performance**.

Pick two. DCPE keeps (1) and (3) and sacrifices (2). A leak-free mode must give up
either (1) — the server stops ranking — or (3) — speed (homomorphic evaluation).
There is no scheme that ranks ciphertexts on the server while revealing *nothing*
about their relative distances: revealing that relation **is** ranking. Any claim
of "fast, leak-free, server-side encrypted ANN at scale" is false, and we will not
make it.

So the honest product framing is a **spectrum**, not a single "encrypted" switch:

```text
 plaintext ───── DCPE ───── client-side (this ADR) ───── [ FHE ]
 server sees     server      server sees only             server computes
 everything;     ranks,      ciphertext + access          on ciphertext;
 fastest         leaks dist. patterns; ZERO leakage;       impractically slow,
                 ordering    server does NOT rank          0.x out of scope
```

Each step trades performance/role for confidentiality. DCPE and this mode are
**complementary points on that spectrum**, not competitors: pick DCPE when the
server must rank at scale and an approximate-distance leak is acceptable; pick
this mode when the server must learn nothing and the working set is small or
already narrowed by a filter.

### The fork — three ways to a leak-free mode

- **(A) Client-side search over AEAD-encrypted vectors.** The server stores only
  XChaCha20-Poly1305 ciphertext — the audited primitive Quiver already ships
  (ADR-0010/0012) — and does **no** distance math. The client fetches the
  entitled / pre-filtered set, decrypts locally, and runs the nearest-neighbour
  search itself. **Truly IND-CPA; zero new cryptography; completable with no
  gaps.** Honest cost: the server does not rank, so the client downloads the
  candidate set — best for small/medium collections or for subsets the server has
  already narrowed by a cleartext filter.
- **(B) Fully homomorphic encryption (FHE) of distances** (e.g. Zama `tfhe-rs`).
  The server computes encrypted distances over FHE ciphertext and returns
  encrypted scores the client decrypts and ranks. The server still "searches" and
  learns nothing. Honest cost: orders of magnitude too slow for interactive ANN
  (≈ seconds per vector), very large ciphertexts, and a heavy new dependency —
  viable only as a tiny "secure exact search," never at the 10M-vector scale that
  is Quiver's memory-frugality headline.
- **(C) Trusted execution environment (TEE / enclave).** Out of scope: it moves
  trust to hardware and a vendor's attestation and opens a cache/timing
  side-channel surface that is a research field in itself — not a property of
  Quiver we can own, audit, and verify.

## Decision

Adopt **(A): client-side AEAD vector encryption**, as a second opt-in,
**off-by-default** vector-encryption mode complementing DCPE. The server stores
**opaque vector ciphertext** it cannot read and over which it does **no distance
computation**; the client decrypts and ranks.

**1. Reuse the audited envelope — no new cryptography.** A reference cipher in
`quiver-crypto` seals a vector's `f32` little-endian bytes with
**XChaCha20-Poly1305** (the same primitive as at-rest and payload encryption),
with a fresh random 192-bit nonce per seal, under a reserved payload key
`__quiver_vec__` and associated data `quiver/vector/v1`. The envelope mirrors the
ADR-0012 payload envelope and records the vector dimension for a sanity check on
open. We implement **no** new primitive — only this thin, well-understood
composition. Because the sealed message is raw bytes (no transcendental floats,
unlike DCPE), the format is **bit-exact across languages**.

**2. The server stores ciphertext plus a placeholder vector, and never ranks.**
An upsert into a client-side collection carries a **zero placeholder vector** of
the declared dimension — so the per-point dimension contract and the
row-addressed store (ADR-0020) are satisfied **unchanged** — plus the sealed
vector blob in the payload. To the engine this is an ordinary point. **There is
no on-disk format change and the `kill -9` crash gate is untouched by
construction.** The server builds **no ANN index** for the collection and
**rejects ranked search**.

**3. The mode is the third value of a `vector_encryption` enum.** Migrate
`Descriptor.encrypted_vectors: bool` to
`vector_encryption: VectorEncryption { None, Dcpe, ClientSide }`. postcard encodes
the bool and the three-variant fieldless enum **identically** for the values that
existed (`false` → `None` = 0, `true` → `Dcpe` = 1), so **existing on-disk DCPE
collections decode unchanged — no data migration** and the descriptor
decode-fallback chain is unchanged. Unlike DCPE, a client-side collection has
**no metric restriction**: the server never ranks, the client ranks with whatever
metric it chooses, and the recorded metric is advisory.

**4. Retrieval is a filtered fetch; the client ranks.** A client-side collection
answers a new **fetch** operation — return points (id + payload, which carries the
sealed vector) matching an optional cleartext payload predicate, bounded by a
limit, with **no vector ranking**. Cleartext filterable fields (ADR-0022
secondary indexes) let the server narrow the returned set, so the client
downloads only the entitled subset, decrypts each blob, computes distances, and
ranks locally.

**5. Ship complete — the SDKs hide the round-trip.** A `VectorCipher` ships
natively in Rust, Python, and TypeScript, plus a client-side **search helper**
that fetches the (filtered) set, decrypts, scores, and returns the top-k — so the
caller still gets a ranked nearest-neighbour API; only the trust/performance
profile differs. There are no half-built edges. Key management is the client's,
documented: use a **dedicated** key (never the at-rest key); losing it makes the
vectors unrecoverable.

**6. Honest by construction, everywhere.** Rustdoc, the design page
(`docs/security/client-side-vectors.md`), the threat model, the README, and every
API description present the spectrum plainly: this mode is semantically secure
(the server sees only ciphertext and access patterns), and its real cost is that
**the server does not rank**, so it suits small/medium collections or
server-pre-filtered subsets. It is never presented as "fast encrypted ANN at
scale."

## What it hides and what it leaks (honest threat model)

**Hides** (against an honest-but-curious server holding only ciphertexts): every
coordinate value and the **entire geometry** — distances, norms, clustering,
nearest-neighbour structure. The vector ciphertext is IND-CPA
(XChaCha20-Poly1305, fresh random nonce per seal), so the same vector sealed twice
is indistinguishable from two unrelated vectors. The server cannot rank, cluster,
or invert.

**Leaks** (by necessity, not cryptographic weakness): the collection's size and
declared dimension; whatever payload fields the client **deliberately leaves
cleartext** to be server-filterable; and **access patterns** — which points are
fetched, how often, in what batches. Hiding access patterns requires ORAM, which
is out of scope and documented as such.

**Versus DCPE:** DCPE leaks the approximate distance relation among *all* vectors
to anyone holding the ciphertexts; this mode leaks none of it. The price is
symmetric — DCPE lets the server rank (fast, scales); this mode does not (the
client ranks a downloaded set). Both compose *with* at-rest encryption; neither
replaces it.

## Implementation

Shipped across the crypto crate, the engine, the network surface, and the SDKs
(v0.11.0):

- **Cipher** (`quiver_crypto::vector`): `VectorCipher` seals a vector's raw
  little-endian `f32` bytes with XChaCha20-Poly1305 (fresh 192-bit nonce, AAD
  `quiver/vector/v1`) under the reserved `__quiver_vec__` payload key. Reuses the
  existing audited AEAD — no new primitive, no new dependency — and the byte layout
  is fixed so it reproduces bit-exactly in other languages.
- **Flag**: `Descriptor.encrypted_vectors: bool` migrated to
  `vector_encryption: VectorEncryption { None, Dcpe, ClientSide }`, byte-compatible
  on disk (`false`→`None`, `true`→`Dcpe`) so existing DCPE collections need no data
  migration; surfaced across REST/gRPC, the MCP tool, and the SDKs.
- **Engine**: a `client_side` collection builds **no** ANN index and **rejects** a
  ranked search; an opaque point is a zero placeholder vector plus the sealed blob,
  so the on-disk format and the `kill -9` crash gate are unchanged. Retrieval is
  `Database::fetch` (optional cleartext filter + limit), exposed as
  `POST /v1/collections/{c}/fetch`, a gRPC `Fetch` RPC, and an MCP `fetch` tool.
- **SDKs**: native `VectorCipher` in Python (`quiver.vector`) and TypeScript
  (`quiver-client/vector`), each validated by a bit-exact cross-language
  known-answer test, plus `search_client_side` / `searchClientSide` helpers that
  fetch, decrypt, and rank.
- **Gate proof**: with encryption-at-rest off, a `client_side` collection rejects a
  ranked query, the client fetches + decrypts + ranks to the true nearest
  neighbour, and the plaintext vectors are **absent** from disk.

## Consequences

- **+** Quiver gains the **strongest** point on the encrypted-search spectrum —
  true semantic security for vectors — shipped open-source, alongside DCPE.
- **+** **Zero new cryptography**: reuses the audited XChaCha20-Poly1305 envelope,
  so the crypto attack surface does not grow.
- **+** **No on-disk format change**; the `kill -9` crash gate is untouched by
  construction (a client-side point is an ordinary zero-vector-plus-payload row).
- **+** Composes with at-rest encryption, RBAC, and payload encryption; orthogonal
  to all three.
- **+** **Byte-compatible flag migration**: existing DCPE collections need no data
  migration; the DCPE cipher and its cross-language KAT are untouched.
- **−** **The server does not rank.** The client downloads the candidate set and
  ranks locally — practical for small/medium collections or server-pre-filtered
  subsets, not for unfiltered 10M-vector top-k. This is the inherent cost of zero
  leakage, documented as a property, not hidden as a bug.
- **−** A placeholder zero vector costs `dim × 4` bytes per point on disk (the
  price of "no format change") — negligible beside the payload blob, but noted.
- **−** Leaks collection size/dimension, the client's chosen cleartext filter
  fields, and access patterns (see threat model).
- **−** Pre-1.0 API refinement: the `encrypted_vectors` flag becomes
  `vector_encryption` across REST/gRPC/MCP and the SDKs. The on-disk format and the
  DCPE cipher/KAT are unchanged; only the flag's spelling changes. Called out in
  the release notes.

## Verification

- **Gate proof** (mirrors ADR-0012 and ADR-0031): boot a server with at-rest
  encryption **off**, so the client-side AEAD is the only thing protecting the
  vectors; create a `client_side` collection; seal vectors client-side and upsert
  (placeholder vector + blob). Assert: (1) the plaintext vector bytes are
  **absent** from disk; (2) a ranked query is **rejected** for the collection (the
  server refuses to rank opaque vectors); (3) every stored vector on disk is the
  identical zero placeholder — the server holds **no geometry at all** (the
  semantic-security proof, by construction); (4) the client fetches the set,
  decrypts, ranks, and recovers the true nearest neighbour end-to-end; (5)
  positive control — a cleartext payload marker **is** on disk, proving the scanner
  works.
- **Round-trip + tamper:** `open(seal(v)) == v` **exactly** (bit-exact: `f32` LE
  bytes through AEAD, no float transforms — stronger than DCPE's tolerance
  round-trip); a wrong key or a flipped ciphertext byte fails the Poly1305 tag.
- **Cross-language KAT:** a vector envelope sealed by the Rust reference decrypts
  **byte-identically** in the Python and TypeScript SDKs — a stronger interop
  guarantee than DCPE's tolerance-based KAT, because the payload is raw bytes.

## Alternatives considered

- **(B) FHE encrypted-distance** — rejected for the 0.x line (above): leak-free and
  the server still ranks, but impractically slow and heavy for ANN at any real
  scale. Revisit later as a tiny experimental "secure exact search" if demand
  appears; it would store FHE ciphertext as opaque payload bytes, so it too needs
  no on-disk structure.
- **(C) TEE / enclave** — out of scope (hardware trust + side channels).
- **DCPE only** — insufficient for users who need zero leakage; DCPE leaks the
  distance-comparison relation by design. The two modes are complementary points
  on the spectrum, not substitutes.
- **Inventing a leak-free distance-preserving transform** — impossible by the
  fundamental tradeoff (a transform the server can rank necessarily reveals the
  ordering) and forbidden by the brief. We will not.
- **Server-side encryption (the server holds the key)** — defeats the purpose; the
  server would see plaintext vectors.
