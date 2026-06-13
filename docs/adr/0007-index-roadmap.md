# ADR-0007: Index roadmap (HNSW → Vamana/IVF)

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Quiver's wedge is memory frugality, but the lowest-latency option (an in-memory graph) is also the simplest to ship first. We need an index strategy that delivers a usable product early and the memory-frugal disk path where the differentiation lives, without betting everything on the riskiest algorithm up front (risk R1). Full design: [`../index/design.md`](../index/design.md).

## Decision

- Indexes are **pluggable per collection** behind a common trait (`build`, `insert`, `search(query, k, params)`, `persist`/`load`), so a collection chooses its point on the recall/latency/memory surface.
- **Phase 1:** ship **HNSW** in-memory — high recall, lowest latency, well-understood — to get an end-to-end usable product.
- **Phase 2:** ship the memory-frugal disk path: a **DiskANN/Vamana** graph (PQ codes in RAM, graph + full vectors on SSD, exact re-rank) as the primary, and **IVF (+PQ / SPANN-style)** as an alternative with a tighter, more predictable RAM profile.
- **R1 fallback:** if a Vamana RAM/recall budget slips in the de-risking spike, IVF+PQ becomes the headline disk index. The claim is only published once measured.

## Consequences

- **+** A usable `v0.1.0` early; the differentiator (disk path) lands in `v0.2.0` with real benchmarks; a documented fallback de-risks the central bet.
- **+** The per-collection trait lets users trade memory for latency explicitly.
- **−** Maintaining multiple index families is real surface area; mitigated by a shared trait, shared distance kernels, and shared quantizers.

## Alternatives considered

- **Disk graph first** — rejected: highest risk and slowest path to a usable product.
- **Only HNSW** — rejected: abandons the memory-frugality wedge.
- **Only IVF** — rejected: lower recall-per-IO than a good graph; kept as the fallback/alternative, not the sole option.
