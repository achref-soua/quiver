# State of Quiver — capability, coherence, security & readiness assessment

> Assessment of the `v0.17.x` line (develop @ `d4e3f06`). This document answers four questions —
> *is Quiver complete as a vector database? is the code/docs/tests coherent? are there security
> vulnerabilities? is it strong enough for RAG / agentic / LLM use?* — and records the gap matrix the
> follow-on roadmap closes against. It is reviewed and updated at each release boundary. Every claim
> here is either backed by a file reference or marked as pending measurement; no number is fabricated.

## Method

A full read of the workspace crates (`quiver-core`, `-index`, `-query`, `-embed`, `-simd`, `-crypto`,
`-server`, `-proto`, `-mcp`, `-import`, `-tui`, `-cli`), the `docs/` set (architecture, ADRs 0001–0039,
security, testing, benchmarks), the `bench/` harness, and the Python/TypeScript SDKs. Capability claims
are cross-checked against the code that implements them and the tests that exercise them.

---

## 1. Capability inventory (what Quiver is today)

| Area | Status | Notes |
|---|---|---|
| **Index families** | ✅ five | HNSW, Vamana, DiskVamana (disk-resident), IVF (+PQ/SQ), ColBERT multi-vector — all *derived* (rebuilt from the store on open), each with incremental updates (HNSW soft-delete, IVF LIRE, Vamana FreshDiskANN StreamingMerge). |
| **Distance metrics** | ✅ three | L2, cosine, dot — AVX2+FMA SIMD kernels with scalar fallback and differential tests (`quiver-simd`). |
| **CRUD / collections** | ✅ | create/delete collection, upsert, delete, get-by-id, batch upsert, forward cursor pagination, multi-vector document upsert/search/delete. |
| **Metadata filtering** | ✅ | keyword + numeric filterable fields (roaring bitmaps, order-preserving keys — ADR-0022); operators `eq/ne/lt/lte/gt/gte/in/exists` + `and/or/not`, dot-path nesting; pre- or post-filter by selectivity; results always exact-re-ranked. |
| **Quantization** | ✅ three | scalar (4×), product (16–32×), binary (32×, Hamming pre-filter) — all selectable per collection. |
| **Durability** | ✅ | encrypted checksummed WAL, atomic manifest, segment storage, compaction, IVF on-disk snapshot + WAL-tail replay; `kill -9` crash gate proven by a subprocess crash-injection test. |
| **Encryption at rest** | ✅ default-on | XChaCha20-Poly1305 on every page + WAL; per-collection DEKs under a master key; crypto-shredding on collection drop. |
| **Client-side encryption** | ✅ | payload sealing; experimental DCPE distance-comparison-preserving vectors (L2-only, opt-in, honestly labelled); semantically secure (IND-CPA) opaque-vector mode with local ranking. |
| **Replication** | ✅ async | leader-follower read replicas (no consensus/failover) — clearly an advanced, single-node-primary feature. |
| **AuthN/Z** | ✅ | API-key bearer + optional mTLS; default-deny RBAC (read⊆write⊆admin) with collection-scope patterns; append-only audit log that never logs secrets. |
| **Interfaces** | ✅ | REST + gRPC + MCP (stdio) + CLI + embeddable Rust API; Python & TypeScript SDKs; LangChain + LlamaIndex adapters (Python). |
| **Migration** | ✅ | offline + live importers for Qdrant / Chroma / pgvector. |

Quiver is **model-agnostic**: it stores caller-supplied vectors and does not generate embeddings (by
design). `quiver-embed` is the embeddable database handle, not an embeddings module.

---

## 2. Gap matrix (vs Qdrant / Milvus / Weaviate / LanceDB / pgvector)

Severity reflects impact on Quiver's stated scope (security-first, memory-frugal, single-node) and on
RAG/agentic competitiveness — not a generic "every DB must have this".

| Gap | Severity | Closes in |
|---|---|---|
| **Hybrid / sparse / full-text (BM25) search + fusion (RRF)** — the largest RAG-competitiveness gap; "hybrid" today means dense + metadata pre-filter only | **High** | Phase 4 (ADR-0043+) |
| **Query cost limits** (caps on `k`, `ef_search`, result size, vector dim) — *claimed in docs, not enforced in code* (see §3) | **High** | Phase 1 (ADR-0040) |
| **Async Python SDK** — sync-only today; awkward for high-concurrency RAG services | **Medium** | Phase 3 (ADR-0042) |
| **Haystack adapter** — LangChain/LlamaIndex exist, Haystack does not | **Medium** | Phase 3 |
| **End-to-end RAG/agentic usage docs** — only API reference exists; no "build a RAG app" guide, chunking patterns, agent loop, or tuning guide | **Medium** | Phase 3 |
| **Reranking surface** — `fetch` + client-side rank works; ColBERT MaxSim is built in; no first-class rerank helper/hook | **Medium** | Phase 5 |
| **Named / multiple vectors per point** (different metrics per field) | Medium | Phase 5 (eval) |
| **Geo filtering** (radius / bbox) | Medium | Phase 5 (eval) |
| **Backup / restore endpoint** (data dir is portable, but no API) | Medium | Phase 5 (eval) |
| **Per-key rate limiting / quotas** | Medium | Phase 5 |
| **Distributed sharding / clustering / consensus** | Low (out of scope) | Roadmap (future ADR) |
| **GPU acceleration** | Low (out of scope) | Roadmap (future ADR) |
| **Lock-free MVCC reads** | Low | Roadmap (future ADR) |
| **Collection aliases** | Low | Roadmap |

"Out of scope" items are deliberate non-goals (R7 in the risk register): Quiver targets single-node /
read-replica memory-frugal deployments, not billion-scale distributed clusters. They are recorded for
honesty, not committed.

---

## 3. Coherence (code vs docs vs tests)

Overall the project is unusually coherent and honest: documented limitations match reality (DCPE
leakage, client-side vectors do not let the server rank, no homomorphic search, single-node scope), and
shipped features are backed by integration + property + fuzz tests with ~91% engine coverage.

**One material mismatch — query cost limits.** The threat model
(`docs/security/threat-model.md:17,60`) and the v0.17.0 audit (`docs/security/audit-0.17.0.md:118`)
state that Quiver enforces "query cost limits (caps on `k`, `ef`, result size, concurrent queries)".
The code does **not**: `SearchBody` (`crates/quiver-server/src/rest.rs:434`) accepts `k: usize` and
`ef_search: usize` and passes them to the search path (`rest.rs:487`) with no validation or clamp, and
`crates/quiver-embed/src/lib.rs:23` itself notes rate limiting is "a later phase". The docs overstate
the code. Phase 1 implements the caps and reconciles the docs to the (then-real) behaviour.

No other doc/code/test mismatch was found. Other "future" controls (per-key rate limiting, connection
limits) are honestly described as deferred in the code comments; only the threat-model/audit phrasing
above asserts them as present.

---

## 4. Security assessment

The security posture is strong for the stated scope. Controls verified in code and tests:

- **Crypto via audited libraries only** (RustCrypto AEAD/KDF, `rustls`/`ring`); AEAD nonce-reuse
  impossible by construction; DCPE built on the same audited cipher stack and fenced behind an opt-in
  flag with published caveats. No home-grown primitive.
- **RBAC** with constant-time API-key comparison (`crates/quiver-server/src/auth.rs:237`), default-deny,
  enforced at a single choke point; out-of-scope collections hidden from listings.
- **Audit log** records actor fingerprint / action / resource / outcome and **never** the secret.
- **TLS** (rustls) for REST + gRPC; non-loopback binds refuse plaintext unless explicitly insecure;
  optional mTLS.
- **Untrusted-input parsers fuzzed** (search-filter wire format, on-disk page + WAL decoders), property
  tests at 8192 cases, crash-injection recovery test.

**Findings:**

- **S1 — Authenticated query-cost DoS (High, fix in Phase 1).** Because `k` / `ef_search` are
  unbounded (see §3), a holder of a valid key (or a leaked/compromised key) can issue
  `k = 10_000_000` / `ef_search = 10_000_000` and force unbounded work; under the single-writer model
  this also blocks other queries. Not remotely exploitable without a key, but a real availability risk
  and the gap behind the §3 coherence issue. Fix: hard caps + a request body-size limit + a query
  timeout.
- **S2 — No vector-dimension / payload-size cap (Low).** A client can upsert a pathologically large
  vector or payload; requires auth and only affects what the caller itself stores. Folded into the
  Phase 1 caps.
- **Already defended / honest (no action):** importer SSRF (operator-CLI URLs only, no network-exposed
  sink), pgvector SQLi (`quote_ident`, never value interpolation), cleartext-credential live-import
  (warns since v0.17.0), `unsafe` blocks (SIMD behind feature detection + mmap, each tested).

**Documented residual risks (by design, not bugs):** plaintext vectors live in process memory while
serving (only `client_side` mode avoids storing them server-side); DCPE leaks approximate distance
ordering; replication to a leader is plaintext on trusted networks; the audit log is append-only, not
cryptographically chained.

---

## 5. RAG / agentic / LLM readiness

**Strong for dense RAG today.** The building blocks exist and are tested: metadata pre-filtered
retrieval, LangChain and LlamaIndex vector-store adapters, an MCP server exposing full CRUD + search +
multi-vector tools (so an agent can create a collection, upsert, filter, and search over stdio), and
ColBERT late-interaction for paragraph/token-level retrieval. A developer can build a working dense-RAG
pipeline against Quiver now.

**Not yet strong for hybrid RAG, and under-documented for both.** Concrete gaps:

- No sparse / BM25 / keyword search and no dense+sparse fusion — the capability incumbents lead on for
  hybrid retrieval (Phase 4).
- Python SDK is synchronous only; no batch/streaming-with-progress, bulk-delete-by-filter, or scan
  iterator (Phase 3).
- No Haystack adapter (Phase 3).
- No end-to-end RAG/agentic *guides* — chunking strategy, retrieve→rerank loop, agent-over-MCP example,
  and an index/quantization tuning guide for RAG workloads (Phase 3). The primitives are present; the
  documented path is not.

---

## 6. Benchmark coverage

The harness (`bench/`, ADR-0037) is ann-benchmarks-style with eight competitor adapters (FAISS,
LanceDB, Chroma, Milvus Lite, Qdrant, pgvector, Weaviate, Quiver) and measures recall@k, QPS, latency
percentiles, build time, index size, and steady-state RSS, with a reproducibility manifest.

**What has actually run:** a full multi-DB **smoke** on SIFTSMALL (10k, 128-d) and a *partial* SIFT1M
(Quiver single-DB + incomplete FAISS/LanceDB). The 65 s build-time regression that smoke surfaced is
already fixed (ADR-0038 batch-WAL sync, 35×).

**Coverage gaps for a deep picture (Phase 2):** larger datasets (SIFT1M, GIST1M-960d, a Deep1M/10M
subset); filtered-search selectivity sweeps; concurrent-client throughput; churn / incremental
workloads (Quiver's differentiator); memory-under-load (RSS during ingest + compaction); cold-open /
restart timing; dense recall↔QPS Pareto curves; quantization tradeoff curves; and hybrid vs dense-only
once Phase 4 lands. Per the risk register, comparative numbers on identical hardware (R6) are publishable
as real results; absolute RSS headline numbers and the 10M disk path (R5) remain
`[reference-hardware-pending]` because a contended VM distorts exactly those two metrics.

---

## 7. Verdict

Quiver is a coherent, secure, well-tested **single-node** vector database with a genuine
memory-frugality and security-by-default wedge, and it is already usable for dense RAG and agentic
(MCP) workloads. It is not "complete" against the full incumbent feature set — the meaningful gaps are
**hybrid/sparse search**, **a few SDK/agent ergonomics**, **deep large-data benchmark coverage**, and
the **one query-cost-limit security/coherence fix**. The follow-on roadmap closes these in order:
Phase 1 (security + coherence), Phase 2 (deep benchmark), Phase 3 (RAG ergonomics + docs), Phase 4
(hybrid/sparse/BM25 — the headline), Phase 5 (reranking + remaining gaps), with distributed/GPU/MVCC
recorded as honest, out-of-scope future work.
