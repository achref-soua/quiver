# Subsystem diagrams

Every major Quiver subsystem, drawn. Sources live in [`docs/diagrams/*.mmd`](./diagrams/)
(Mermaid) and render to committed SVGs in [`docs/assets/diagrams/`](./assets/diagrams/) via
`just diagrams`. For the narrative tour with 31 hand-drawn figures, see the field guide
[`docs/quiver-explained.pdf`](./quiver-explained.pdf); for the C4 views and crate DAG, see
[`docs/architecture/`](./architecture/).

## Storage & durability

### Payload storage — row-addressed columns & paged heaps

![Payload storage](assets/diagrams/payload-storage.svg)

### Online snapshot — consistent backup

![Snapshot isolation](assets/diagrams/snapshot-isolation.svg)

### Lock-free MVCC reads

![MVCC reads](assets/diagrams/mvcc-reads.svg)

### Deferred index rebuild — state machine

![Deferred rebuild](assets/diagrams/deferred-rebuild.svg)

## Indexing & retrieval

### Secondary (metadata) indexes & filter evaluation

![Secondary indexes](assets/diagrams/secondary-indexes.svg)

### BM25 sparse / hybrid retrieval

![BM25 sparse](assets/diagrams/bm25-sparse.svg)

### Multi-vector / ColBERT late interaction

![Multi-vector MaxSim](assets/diagrams/multivector-maxsim.svg)

### Vector quantization — PQ / scalar / binary

![Quantization](assets/diagrams/quantization.svg)

### Server-side embedding hooks

![Server-side embedding](assets/diagrams/server-embedding.svg)

## Query safety

### Query cost limits & DoS protection

![Query cost limits](assets/diagrams/query-cost-limits.svg)

## Distributed

### Dynamic cluster membership

![Dynamic membership](assets/diagrams/dynamic-membership.svg)

### gRPC streaming APIs

![gRPC streaming](assets/diagrams/grpc-streaming.svg)

## Security

### Envelope key hierarchy & KMS

![Key hierarchy and KMS](assets/diagrams/key-hierarchy-kms.svg)

### TLS / mTLS termination

![TLS and mTLS](assets/diagrams/tls-mtls.svg)

### Authentication & RBAC

![Auth and RBAC](assets/diagrams/authz-rbac.svg)

### Append-only audit log

![Audit log](assets/diagrams/audit-log.svg)

## Operability

### Observability — metrics, tracing, OTLP

![Observability](assets/diagrams/observability.svg)

## Integration

### Migration importers (Qdrant / Chroma / pgvector)

![Migration importers](assets/diagrams/migration-import.svg)
