# Decision records (ADRs)

Every significant, hard-to-reverse decision is captured as a short, numbered
**Architecture Decision Record** so future readers understand *why* the system is
shaped the way it is. ADRs are immutable once Accepted; we supersede rather than
edit, and numbers are never reused.

Browse the full set in the repository:
[`docs/adr/`](https://github.com/achref-soua/quiver/tree/main/docs/adr) (the
[index](https://github.com/achref-soua/quiver/blob/main/docs/adr/README.md) lists
every record with its status).

## Key records by theme

**Foundations**
- [0001](https://github.com/achref-soua/quiver/blob/main/docs/adr/0001-language-and-workspace.md) Language & workspace · [0004](https://github.com/achref-soua/quiver/blob/main/docs/adr/0004-on-disk-format.md) On-disk format · [0005](https://github.com/achref-soua/quiver/blob/main/docs/adr/0005-durability-and-recovery.md) Durability & crash recovery · [0006](https://github.com/achref-soua/quiver/blob/main/docs/adr/0006-concurrency-model.md) Concurrency

**Indexing & storage**
- [0007](https://github.com/achref-soua/quiver/blob/main/docs/adr/0007-index-roadmap.md) Index roadmap · [0008](https://github.com/achref-soua/quiver/blob/main/docs/adr/0008-quantization.md) Quantization · [0019](https://github.com/achref-soua/quiver/blob/main/docs/adr/0019-disk-index-format.md) Disk-resident index · [0020](https://github.com/achref-soua/quiver/blob/main/docs/adr/0020-row-addressed-segment-storage.md) Row-addressed segments
- Incremental updates: [0023](https://github.com/achref-soua/quiver/blob/main/docs/adr/0023-incremental-in-place-updates.md) IVF · [0026](https://github.com/achref-soua/quiver/blob/main/docs/adr/0026-hnsw-incremental-delete.md) HNSW · [0033](https://github.com/achref-soua/quiver/blob/main/docs/adr/0033-graph-incremental-freshdiskann.md) graph FreshDiskANN · [0025](https://github.com/achref-soua/quiver/blob/main/docs/adr/0025-durable-incremental-index.md) durable on-disk index
- Multi-vector: [0028](https://github.com/achref-soua/quiver/blob/main/docs/adr/0028-multi-vector-late-interaction.md) late interaction · [0034](https://github.com/achref-soua/quiver/blob/main/docs/adr/0034-multivector-followups.md) follow-ups

**Security**
- [0010](https://github.com/achref-soua/quiver/blob/main/docs/adr/0010-crypto-envelope-aead.md) Envelope encryption & AEAD · [0011](https://github.com/achref-soua/quiver/blob/main/docs/adr/0011-authn-authz-tenancy.md) AuthN/Z & tenancy · [0012](https://github.com/achref-soua/quiver/blob/main/docs/adr/0012-client-side-encryption.md) Client-side payload encryption
- Vector encryption: [0031](https://github.com/achref-soua/quiver/blob/main/docs/adr/0031-dcpe-vector-encryption.md) DCPE · [0032](https://github.com/achref-soua/quiver/blob/main/docs/adr/0032-client-side-vector-encryption.md) client-side opaque vectors · [0035](https://github.com/achref-soua/quiver/blob/main/docs/adr/0035-docs-site-and-dcpe-hardening.md) docs site + DCPE hardening

**Platform & integration**
- [0024](https://github.com/achref-soua/quiver/blob/main/docs/adr/0024-migration-importers.md) Migration importers · [0027](https://github.com/achref-soua/quiver/blob/main/docs/adr/0027-live-migration-connectors.md) live connectors · [0030](https://github.com/achref-soua/quiver/blob/main/docs/adr/0030-leader-follower-replication.md) replication · [0018](https://github.com/achref-soua/quiver/blob/main/docs/adr/0018-sdk-and-integration-strategy.md) SDK strategy · [0015](https://github.com/achref-soua/quiver/blob/main/docs/adr/0015-ci-policy.md) CI policy
