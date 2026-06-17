# Concepts

A short tour of the nouns you will meet everywhere in Quiver.

## Collections

A **collection** is a named set of vectors of a fixed dimension, with a fixed
distance **metric** (`cosine`, `dot`, or `l2`) and an index configuration. You
create a collection, upsert points into it, and search it. Each collection has its
own data-encryption key (envelope encryption), so dropping a collection
**crypto-shreds** it — its key is destroyed and its data becomes unrecoverable.

## Points, vectors, and payloads

A **point** is an `id`, a **vector**, and an optional JSON **payload**. The vector
is what you search by; the payload carries metadata (`{"tag": "x", "year": 2024}`).
Payload fields can be declared **filterable** so they participate in hybrid search,
and they can be **client-side-encrypted** so the server stores only ciphertext for
the sensitive fields while cleartext siblings stay filterable.

## Metrics

- **`cosine`** — angle between vectors (normalize-and-dot); the usual choice for
  text embeddings.
- **`dot`** — inner product; for models trained with dot-product objectives.
- **`l2`** — Euclidean distance; required by the DCPE encrypted-search mode (its
  secret scaling preserves L2 ordering, not cosine/dot).

## Indexes

The index is how Quiver answers approximate nearest-neighbour queries quickly. Pick
per collection (see [Indexing & memory frugality](features/indexing.md)):

- **HNSW** — a fast in-memory graph; the default.
- **IVF** — inverted-file clustering; pairs well with quantization.
- **Vamana** — the DiskANN graph, in-memory or **disk-resident**.
- **`disk_vamana`** — the memory-frugality wedge: the graph and full-precision
  vectors live in the encrypted on-disk index while only compact PQ codes stay in
  RAM.
- **`colbert`** — an opt-in ColBERTv2/PLAID token-pool index for multi-vector
  collections.

Every index supports **incremental updates**, so a streaming workload never pays an
`O(N)` rebuild per write.

## Quantization

Optional compression of the stored vectors — **product**, **scalar**, or **binary**
— trading a little recall for a large drop in memory. The
[tradeoff table](features/indexing.md) documents the knobs.

## Filtering & hybrid search

A search can carry a **filter** over filterable payload fields. Quiver's planner
either pre-filters to an exact scan (when the filter is selective) or post-filters
the ANN results — so metadata constraints compose with vector similarity.

## Multi-vector documents

A `multivector` collection stores each **document** as a *set* of token vectors and
ranks documents by **MaxSim** late interaction (ColBERT). See
[Multi-vector / late interaction](features/multi-vector.md).

## Tenancy, keys, and access control

Authentication is by **API key**; authorization is **default-deny RBAC** with roles
(`read` ⊆ `write` ⊆ `admin`) and collection **scopes** (exact names or a
trailing-`*` prefix for per-namespace isolation). Optional **mutual TLS** adds a
second factor, and an append-only **audit log** records every mutating or
administrative action. See [Self-hosting & configuration](self-hosting.md).

## Embeddable vs server

One binary runs three ways: an in-process **embeddable** database, a **server**
(REST + gRPC), and an **MCP server** (stdio) so AI agents can drive it. A data
directory is portable between them.
