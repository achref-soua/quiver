# ADR-0035: Documentation site, DCPE hardening, and a native TypeScript cipher

- **Status:** Accepted
- **Date:** 2026-06-17
- **Deciders:** Achref Soua

## Context

`v0.15.0` is the last build-here increment before the `v1.0.0` launch. It bundles
the documentation and packaging polish the launch needs with the two
honestly-deferred DCPE follow-ups that have been carried since ADR-0031:

1. **A documentation site.** Quiver has a deep `docs/` tree — ADRs, the index
   design, the wire protocol, the security docs, the architecture C4 views — but no
   browsable site that ties them into a reader's journey (concepts → quickstart →
   self-hosting → features → API/SDKs → security → architecture). The launch DoD
   (`docs/roadmap.md`) requires a live docs site.

2. **Clean-clone quickstart polish.** The launch DoD also requires that a fresh
   `git clone` builds, runs, and answers a first query "in minutes." The README
   quickstart must be verified end-to-end against a clean checkout and any drift
   fixed. Install is **from source** (`cargo install --path crates/quiver-cli`,
   binary name `quiver`) because the crates.io `quiver-cli` name is an unrelated
   squatted crate and the SDKs are unpublished (ADR/roadmap note, `v0.12.0`).

3. **A native TypeScript `DcpeCipher`.** ADR-0031 shipped DCPE with a Rust
   reference cipher (`quiver_crypto::dcpe`) and a native Python port
   (`quiver.dcpe`), validated against each other by a cross-language known-answer
   test (KAT). The TypeScript SDK got only the `vector_encryption="dcpe"` **create
   flag** — there is no native TS cipher, so a JS/TS client cannot actually encrypt
   vectors or queries. ADR-0031 recorded this as a follow-up.

4. **The two deferred DCPE Scale-And-Perturb (SAP) hardening steps.** ADR-0031
   implemented the core SAP construction (scale `s`, bounded d-ball perturbation
   `λ`, `c = s·m + λ`, HMAC tag) but **honestly deferred** the paper's two
   additional hardening steps (Fuchsbauer, Ghosal, Hauke & O'Neill, *"Approximate
   Distance-Comparison-Preserving Symmetric Encryption,"* IACR ePrint 2021/1666,
   SCN 2022; the scheme behind IronCore's Cloaked AI):
   - a **secret component shuffle** — a key-derived permutation of the vector's
     dimensions; and
   - **plaintext-distribution normalisation** — mapping the plaintext distribution
     to a canonical one before encrypting, so the fixed-radius perturbation gives
     consistent protection.

The hard constraint shared by 3 and 4: **never invent cryptography.** Every
primitive must come from an audited library (RustCrypto on the Rust side;
`cryptography` on the Python side; `@stablelib` / `@noble` on the TS side, matching
the optional-peer-dependency pattern `vector.ts` already uses). The DCPE cipher is
client-side and the server stores DCPE vectors as ordinary L2 vectors, so **none of
the cipher work changes the on-disk format or the `kill -9` crash gate.**

## Decision

### Part 1 — Documentation site: mdBook under `apps/docs`

Use **mdBook** (pure Rust, a single binary, no Node toolchain) rather than
Fumadocs/Nextra. It matches Quiver's frugal, pure-Rust ethos, builds in CI without
`node_modules` or a JS build step, and keeps the docs site in the same toolchain as
the rest of the repo. The richer React-based options were rejected only for the
Node/pnpm build weight they add to an otherwise dependency-lean project.

The book lives at `apps/docs/` (`book.toml` + `src/`). To avoid duplicating the
canonical `docs/` content, deep chapters **reuse the existing files** via mdBook's
`{{#include}}` preprocessor (the security deep-dives, the architecture/index
design, the migration and replication guides, the REST/gRPC and MCP references);
the connective chapters (introduction, concepts, quickstart, self-hosting,
features overview, SDK usage) are authored fresh in `src/`. A `just docs` recipe
runs `mdbook build`; building the site is **not** added to `just verify` (the
authoritative gate stays the Rust pipeline, ADR-0015) so the gate keeps no new
dependency, but the book is verified to build locally.

### Part 2 — Clean-clone quickstart polish

Walk a fresh checkout through build → run → first query, reconcile the README
quickstart and the new `quickstart`/`self-hosting` docs pages with what actually
runs, and fix any drift. Keep install from source and the binary name `quiver`.

### Part 3 — Native TypeScript `DcpeCipher`

Port `quiver_crypto::dcpe` to `sdks/typescript/src/dcpe.ts`, exported at the
`quiver-client/dcpe` subpath (mirroring `quiver-client/vector` and
`quiver-client/encryption`) so the core client stays dependency-free. The
primitives — ChaCha20 (the CSPRNG keystream), HKDF-SHA256, HMAC-SHA256, SHA-256 —
come from audited `@stablelib` packages added as **optional peer dependencies**,
exactly as `@stablelib/xchacha20poly1305` is for `vector.ts`. A new TS KAT decrypts
a Rust-produced vector (tag verifies bit-exact; plaintext recovered within float
tolerance), modelled on `sdks/python/tests/test_dcpe.py::test_kat_matches_the_rust_reference`.

The 64-bit keystream words are read with `BigInt` so the `u64` arithmetic
(`>> 11`, `% n`) matches Rust/Python exactly; the `[0,1)` mapping and the
Box-Muller normals then run in IEEE-754 `f64` (`number`), matching within ULPs.

### Part 4 — DCPE SAP hardening (cipher v2): component shuffle + normalisation

Harden the cipher in all three implementations (Rust reference, Python, the new TS
port) and regenerate the cross-language KAT. DCPE is experimental and pre-1.0, so a
**breaking cipher change is acceptable** — and there is **no on-disk format
change** (DCPE vectors are and remain ordinary stored L2 vectors). To make the
break loud rather than silent, the integrity-tag domain is bumped to
`quiver/dcpe/v2/tag`, so a `v1` ciphertext fails a `v2` integrity check with a
clear error instead of decrypting to garbage.

The encryption pipeline becomes, applied to a plaintext `m ∈ ℝ^d` (each step
**must** preserve the L2 distance-comparison ordering, since the untrusted server
ranks on the ciphertext):

1. **Normalise (optional, ordering-preserving).** A **fixed global affine
   transform** `m₁ = (m − μ) · α`, where `μ ∈ ℝ^d` is a per-dimension shift vector
   (default `0`) and `α > 0` is a **single scalar** scale (default `1`). A uniform
   per-coordinate shift cancels in any difference (`‖(a−μ)−(b−μ)‖ = ‖a−b‖`) and a
   single positive scalar scales every distance by the same `α` (`‖α(a−b)‖ =
   α‖a−b‖`), so the ordering is preserved exactly and the transform is invertible
   (`m = m₁/α + μ`). The transform is **fixed at cipher construction** — supplied
   once by the caller from a one-time measurement of their corpus (its
   per-dimension mean and a global RMS radius) or key-derived — never recomputed
   per batch.

2. **Shuffle (key-derived permutation `π`).** Permute the `d` components with a
   permutation derived **from the key alone** (a new HKDF sub-key
   `quiver/dcpe/v2/shuffle`, a ChaCha20 CSPRNG with a fixed all-zero IV, and a
   fully-specified Fisher–Yates). L2 distance is **invariant** under any
   permutation of coordinates, so applying the *same* `π` to every vector and every
   query preserves all pairwise distances exactly (no recall loss); the inverse
   permutation reverses it on decrypt. The permutation depends only on `(key, d)`,
   so it is identical for data and queries and reproducible across languages.

3. **Scale-and-perturb (unchanged from ADR-0031).** `c = s · π(m₁) + λ`, with `λ`
   the uniform d-ball perturbation of radius `(s/4)·β` drawn from `(prf_key, iv)`.

4. **Tag (domain bumped).** HMAC-SHA256 over `(quiver/dcpe/v2/tag ‖ β ‖ iv ‖ c)`.

Decryption verifies the tag, re-derives `λ`, then reverses the pipeline:
`m = T⁻¹(π⁻¹((c − λ)/s))`.

**What the shuffle buys:** it hides the axis-alignment of the ciphertext — which
ciphertext coordinate corresponds to which plaintext coordinate — frustrating
coordinate-wise analysis and the simplest embedding-inversion shortcuts, at zero
recall cost (it is an exact isometry). A key-derived permutation is the
shuffle the paper and the prompt specify; a full key-derived orthonormal rotation
would hide more but cannot be made bit-reproducible across languages in `f32`, so a
permutation is both the specified and the more honest (KAT-exact) choice.

**What normalisation buys, and its honest limit.** Normalisation makes the
approximation factor `β` mean the same thing across datasets: the perturbation
radius `(s/4)·β` is fixed in absolute terms, so unless the data occupies a
canonical scale, the same `β` over- or under-protects. A global recentre + rescale
canonicalises the cloud's centroid and overall scale (the dominant distributional
signals) while preserving the search ordering.

It **cannot** do per-axis variance whitening. Standardising each dimension
independently (`m_i' = (m_i − μ_i)/σ_i` with a *per-axis* `σ_i`) is the most
effective normalisation — but a per-axis scaling is an anisotropic transform that
**re-weights the dimensions in the L2 distance**, so it does **not** preserve the
distance-comparison ordering the untrusted server relies on. There is therefore a
genuine, unavoidable trade-off: **full per-axis whitening is incompatible with
server-side distance-preserving search.** We implement the strongest normalisation
that *is* compatible — a fixed global affine (full per-axis shift + a single global
scale) — and document the limit plainly rather than ship an anisotropic transform
that would silently break recall. This is the limit the design was required to
confront, stated honestly.

ADR-0031 is left **immutable** (it records the original `v1` scheme as shipped);
this ADR records the hardened `v2` cipher as the new state.

## Crash-safety & compatibility

- **No on-disk format change.** DCPE vectors are stored as ordinary L2 vectors; the
  cipher is entirely client-side. The `kill -9` crash gate is untouched by
  construction (the ADR-0031 stance).
- **Breaking cipher change, bounded.** `v1`-encrypted vectors are not
  `v2`-compatible. DCPE is experimental and off by default; the `v2/tag` domain
  makes a version mismatch fail closed (an integrity error), and the break is
  documented in the ADR, `docs/security/dcpe.md`, and the SDK docs.
- **No server/wire change.** REST/gRPC/MCP already take `vector_encryption="dcpe"`
  and store the ciphertext as an L2 vector; shuffle and normalisation are
  client-side, so no surface beyond the three ciphers and the KAT changes.

## Consequences

- **+** A browsable docs site (mdBook) covering the full reader journey, built from
  the canonical docs with no duplication and no Node toolchain.
- **+** A verified clean-clone quickstart — a launch-DoD item closed.
- **+** TypeScript clients can finally use DCPE end-to-end (encrypt vectors and
  queries), validated bit-for-bit against the Rust reference by a KAT — closing the
  last DCPE SDK gap.
- **+** The cipher gains the two published hardening steps to the extent they are
  compatible with searchable encryption: axis-alignment hiding at zero recall cost,
  and consistent `β` semantics via global normalisation.
- **−** A breaking DCPE cipher change (acceptable: experimental, off by default, no
  on-disk impact, fail-closed on version mismatch).
- **−** An honest, documented limitation: DCPE still cannot do per-axis whitening
  without sacrificing server-side searchability, and remains **not** semantically
  secure and **L2-only** (unchanged from ADR-0031). DCPE composes with, but does
  not replace, encryption-at-rest or the semantically secure client-side mode
  (ADR-0032).
- **−** mdBook is a new (dev-time, optional) build tool; it is deliberately kept
  out of `just verify` so the authoritative gate gains no dependency.

## Alternatives considered

- **Fumadocs/Nextra for the docs site** — richer (search, MDX components) but adds
  a Node/pnpm build and `node_modules` to a pure-Rust + minimal-SDK repo; rejected
  for weight against the frugal ethos.
- **A key-derived orthonormal rotation instead of a permutation** — hides more
  (mixes coordinates rather than relabelling them) and is still an L2 isometry, but
  `f32` matrix multiplication is not bit-reproducible across languages, breaking the
  KAT-exactness guarantee; the prompt and the literature specify a permutation, so a
  permutation it is.
- **Per-axis (anisotropic) whitening** — the most effective normalisation, but it
  breaks the L2 distance-comparison ordering and so is **incompatible** with
  untrusted-server search; rejected, and the incompatibility is documented as a
  limit rather than worked around.
- **A data-adaptive normalisation recomputed as data arrives** — would diverge
  between the encrypt-time and query-time clients and leak the evolving
  distribution; rejected in favour of a transform fixed at cipher construction.
- **A non-breaking, versioned cipher carrying both `v1` and `v2`** — needless
  complexity for an experimental, off-by-default, pre-1.0 feature with no on-disk
  footprint; a clean breaking change with a fail-closed domain is simpler and
  honest.

## Implementation

All four pieces shipped in `v0.15.0`.

- **Docs site.** `apps/docs` is an mdBook (`book.toml` + `src/`) with a `just docs`
  recipe. The connective chapters (introduction, concepts, quickstart,
  self-hosting, the feature overviews, SDK usage, the ADR index) are authored in
  `src/`; the deep reference chapters reuse the canonical top-level `docs/` via
  mdBook `{{#include}}` (the four security docs, the architecture/index design, and
  the migration / replication / REST-gRPC / MCP references), so there is a single
  source of truth. The build stays out of `just verify` (ADR-0015); the built
  `book/` is git-ignored.
- **Quickstart polish.** The clean-clone build → run → first-query path was verified
  end to end against a fresh build (CLI surface, default ports 6333/6334, the demo
  key, and the Python/TypeScript SDK signatures all match; a live boot with
  encryption-at-rest on created a collection, upserted, and searched through the
  Python SDK). No drift was found; the README now points into the docs site.
- **Native TypeScript `DcpeCipher`.** `sdks/typescript/src/dcpe.ts`, exported at the
  `quiver-client/dcpe` subpath, ports the cipher using audited `@stablelib` packages
  (`chacha`, `hkdf`, `hmac`, `sha256`) as optional peer dependencies. The 64-bit
  keystream words are read with `BigInt` so the `u64` arithmetic matches Rust/Python
  exactly.
- **DCPE v2 hardening.** The Rust reference (`quiver_crypto::dcpe`), the Python port
  (`quiver.dcpe`), and the new TS cipher all gained: a key-derived Fisher–Yates
  **component shuffle** (HKDF sub-key `quiver/dcpe/v2/shuffle`, a ChaCha20 CSPRNG
  with a fixed zero IV) applied identically to every vector and query; an optional
  ordering-preserving global affine **`Normalization`** (a per-dimension shift plus a
  single positive scalar scale); and the integrity-tag domain bumped to
  `quiver/dcpe/v2/tag`. Per-axis variance whitening is **not** implemented — it is
  anisotropic and would break the L2 distance-comparison ordering — and that limit is
  documented in each cipher's module docs and in `docs/security/dcpe.md`. The cipher
  is client-side, so there was no server/wire change and no on-disk format change.

## Verification

- **DCPE v2 cross-language KAT.** The Rust, Python, and TS ciphers are validated
  against a single known-answer vector produced by the Rust reference: the
  HMAC-SHA256 tag verifies **bit-exact** (HKDF + HMAC are deterministic across
  languages) and the plaintext is recovered within float tolerance after
  un-shuffling — proving all three agree, including the derived permutation
  (`[2,6,1,5,7,0,4,3]` for the KAT key at `d=8`). Each language also tests the
  shuffle (a valid, deterministic, key-dependent permutation; an effect at `β=0`),
  normalisation (round-trip, nearest-neighbour preservation, and rejection of bad
  parameters / dimension mismatch), and the prior DCPE properties (which still hold,
  since the shuffle and normalisation are transparent when the same cipher encrypts
  and queries). `just verify`, `just test-py` (50), and `just test-ts` (43) are green.
- **Docs site.** `mdbook build apps/docs` is clean; the embedded canonical content
  (the DCPE spec, the index design, migration, the threat model) renders and search
  is built.

## References

- E. Fuchsbauer, R. Ghosal, N. Hauke, A. O'Neill. *Approximate
  Distance-Comparison-Preserving Symmetric Encryption.* IACR ePrint 2021/1666; SCN
  2022. (The Scale-And-Perturb scheme; the shuffle and normalisation hardening.)
- IronCore Labs, *Cloaked AI* — the SAP scheme applied to embedding vectors.
- ADR-0031 (DCPE `v1`, immutable), ADR-0032 (semantically secure client-side
  vectors), ADR-0012 (client-side payload encryption — the optional-peer-dependency
  SDK pattern), ADR-0015 (CI policy — `just verify` is the gate), ADR-0018 (SDK &
  integration strategy).
