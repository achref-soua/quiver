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
| [0003](0003-serialization.md) | Serialization formats | Accepted | 0 |
| [0004](0004-on-disk-format.md) | On-disk format | Accepted | 0 |
| [0005](0005-durability-and-recovery.md) | Durability & crash recovery | Accepted | 0 |
| [0006](0006-concurrency-model.md) | Concurrency model | Accepted | 0 |
| [0007](0007-index-roadmap.md) | Index roadmap (HNSW → Vamana/IVF) | Accepted | 0 |
| [0008](0008-quantization.md) | Quantization strategy | Accepted | 0 |
| [0009](0009-simd-kernels.md) | SIMD distance kernels | Accepted | 0 |
| [0010](0010-crypto-envelope-aead.md) | Crypto: envelope encryption & AEAD | Accepted | 0 |
| [0011](0011-authn-authz-tenancy.md) | AuthN/Z & tenant isolation | Accepted | 0 |
| [0012](0012-client-side-encryption.md) | Client-side encryption & trust boundary | Accepted | 0 |
| [0013](0013-config-and-secure-defaults.md) | Configuration & secure defaults | Accepted | 0 |
| [0014](0014-observability.md) | Observability | Accepted | 0 |
| [0015](0015-ci-policy.md) | CI policy — manual-only + local verify gate | Accepted | 0 |
| [0016](0016-license-agpl.md) | License — AGPL-3.0 | Accepted | 0 |
| [0017](0017-error-handling.md) | Error handling | Accepted | 0 |
| [0018](0018-sdk-and-integration-strategy.md) | SDK & integration strategy | Accepted | 0 |
| [0019](0019-disk-index-format.md) | Disk-resident index format (DiskANN on encrypted pages) | Accepted | 2 |
| [0020](0020-row-addressed-segment-storage.md) | Row-addressed segment storage (`.vec`/`.pay`/`.dir`, mmap) | Accepted | 2 |
| [0021](0021-tombstones-and-compaction.md) | Tombstones (roaring `.del`) and compaction | Accepted | 2 |
| [0022](0022-secondary-indexes.md) | Secondary indexes (`.sec`, order-preserving keys) | Accepted | 2 |

Phase-0 ADRs (0001–0018) are Accepted; Phase-2 decisions begin at 0019. New decisions take the next free number; superseded ADRs are marked as such — never deleted or renumbered.
