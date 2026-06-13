# Quiver — Risk Register

Likelihood / Impact on a Low–Medium–High scale. Each risk has a mitigation and an early signal that tells us to act. Reviewed at every phase boundary.

| ID | Risk | L | I | Mitigation | Early signal |
|---|---|---|---|---|---|
| **R1** | **Disk-resident index** doesn't hit the memory/recall target (the headline claim) | M | H | Analytical RAM budget + recall model in Phase 0; `.scratch` spike before Phase 2 commits; prove with real 10M+ benches; **fallback** to IVF+PQ if a Vamana RAM budget slips | Spike shows RAM/vector above budget at target recall |
| **R2** | SIMD kernels wrong or non-portable across CPUs | M | M | Scalar reference + **differential tests** (SIMD == scalar for every dtype/dim); runtime feature detection + scalar fallback; `criterion` benches; dispatch CI across feature levels | A differential test diverges; a target lacks AVX2/NEON |
| **R3** | **Crash mid-write corrupts or loses acknowledged data** | M | H | WAL + per-page checksums; **kill-mid-write tests** (SIGKILL during write → reopen → assert durability & no torn pages); `proptest` recovery; fuzz the on-disk parser | Any recovery test finds a torn/lost committed record |
| **R4** | Cryptographic trust-boundary error | L | H | **Audited libraries only**; peer-readable threat model; AEAD nonce-reuse impossible by construction; known-answer test vectors; client-side-encryption boundary documented; experimental DCPE fenced behind a flag with caveats | A review/test finds plaintext where ciphertext is expected |
| **R5** | Dev-box resource limits skew or block benchmarks (box is shared) | H | M | Run/report benchmarks on **documented reference hardware** with full specs; small smoke datasets in CI, large runs manual; **never fabricate numbers** | OOM or thermal throttling during a bench run |
| **R6** | Benchmark credibility — unfair or irreproducible comparison | M | H | Reproducible methodology doc; identical hardware; pinned datasets/seeds; publish raw numbers + exact configs; tune competitors per their own guidance; invite reproduction | Numbers can't be reproduced from the documented steps |
| **R7** | Scope creep dilutes the wedge | M | M | Roadmap enforces single-node-first; **non-goals stated explicitly**; replication/DCPE/multi-vector gated to Phase 4/stretch | A PR adds breadth before the current phase's DoD is met |
| **R8** | Supply-chain / dependency vulnerability or license issue | M | M | `cargo deny` (advisories + licenses + bans), `cargo audit`, minimal deps, SBOM via Syft, pinned versions, `gitleaks` pre-commit | `deny`/`audit` flags an advisory or disallowed license |
| **R9** | `unsafe` defect (UB, aliasing, OOB) | L | H | Minimize `unsafe`; every block carries a `// SAFETY:` note + a test; **Miri** in CI for `core`/`simd`; ASan on fuzz targets | Miri/ASan reports UB; a fuzz crash in unsafe code |
| **R10** | Index concurrency bug under concurrent read/write | M | H | Documented concurrency model (ADR-0006); **`loom`** model-checked tests; start single-writer/multi-reader; epoch-based reclamation | A loom test or stress test deadlocks/races |
| **R11** | Single-maintainer bus factor / sustainability | M | M | Docs-first, ADRs for the *why*, small reviewable PRs, clean history; honest, bounded scope | Design intent lives only in one person's head |

## Detail on the top three

### R1 — Disk-resident index memory/recall (the project's central bet)

The whole memory-frugality claim rests on a DiskANN/Vamana-style index serving high recall while keeping only PQ-compressed vectors (and the graph's working set) in RAM. **De-risk order:** (1) in Phase 0, write the analytical RAM budget — bytes/vector for PQ codes + graph adjacency resident set — and a recall model from the cited papers; (2) before Phase 2 implementation, a `.scratch` spike on a public 1–10M dataset measures real recall vs RAM for a chosen `(M, PQ subspaces, beam width)`; (3) Phase 2 proves it at 10M+. **If the budget slips**, fall back to IVF + PQ (SPANN-style inverted lists), which trades graph quality for a tighter, more predictable memory profile. The claim is only published once measured.

### R3 — Crash recovery (a database's first duty)

Durability is non-negotiable: an acknowledged write must survive `kill -9`. The WAL is the source of truth; segments are checkpoints. The crash-recovery test harness forks a writer, SIGKILLs it at randomized points (including mid-page-write and mid-WAL-append), reopens the store, and asserts (a) every acknowledged write is present, (b) no partially-written page is mistaken for valid (checksum catches it), (c) the WAL replays idempotently. This gates `v0.1.0`.

### R6 — Benchmark credibility (reputation is the asset)

The project's credibility is the benchmark table. A single unfair or irreproducible number discredits everything. Therefore: competitors are configured per their own recommended settings, on the same box, same datasets, same recall operating point; we publish the exact configs, the harness, and raw CSVs; memory is measured the same way for all systems (RSS at steady state). Adjectives are banned from the README — only numbers, with a link to reproduce.
