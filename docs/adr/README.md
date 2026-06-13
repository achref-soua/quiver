# Architecture Decision Records

Every significant, hard-to-reverse decision is captured here as a short, numbered record so future readers understand *why* the system is shaped the way it is.

## Format

Each ADR is `NNNN-kebab-title.md` with:

- **Status** — Proposed · Accepted · Superseded by ADR-XXXX · Deprecated
- **Date** and **Deciders**
- **Context** — the forces and constraints in play
- **Decision** — what we will do
- **Consequences** — the trade-offs we accept, good and bad
- **Alternatives considered** — what we rejected and why

ADRs are immutable once Accepted; we supersede rather than edit. Numbers are stable and never reused.

## Index

| # | Title | Status | Phase |
|---|---|---|---|
| [0001](0001-language-and-workspace.md) | Language and workspace layout | Accepted | 0 |
| [0002](0002-async-runtime.md) | Async runtime — Tokio | Accepted | 0 |
| 0003 | Serialization formats | Proposed (PR-B) | 0 |
| 0004 | On-disk format | Proposed (PR-B) | 0 |
| 0005 | Durability & crash recovery | Proposed (PR-B) | 0 |
| 0006 | Concurrency model | Proposed (PR-B) | 0 |
| 0007 | Index roadmap (HNSW → Vamana/IVF) | Proposed (PR-C) | 0 |
| 0008 | Quantization strategy | Proposed (PR-C) | 0 |
| 0009 | SIMD distance kernels | Proposed (PR-C) | 0 |
| 0010 | Crypto: envelope encryption & AEAD | Proposed (PR-D) | 0 |
| 0011 | AuthN/Z & tenant isolation | Proposed (PR-D) | 0 |
| 0012 | Client-side encryption & trust boundary | Proposed (PR-D) | 0 |
| 0013 | Configuration & secure defaults | Proposed (PR-D) | 0 |
| 0014 | Observability | Proposed (PR-D) | 0 |
| [0015](0015-ci-policy.md) | CI policy — manual-only + local verify gate | Accepted | 0 |
| [0016](0016-license-agpl.md) | License — AGPL-3.0 | Accepted | 0 |
| [0017](0017-error-handling.md) | Error handling | Accepted | 0 |
| [0018](0018-sdk-and-integration-strategy.md) | SDK & integration strategy | Accepted | 0 |

Rows marked *Proposed (PR-X)* are reserved numbers whose records land with the matching Phase-0 design PR.
