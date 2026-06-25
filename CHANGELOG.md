# Changelog

All notable changes to Quiver are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Quiver is pre-1.0: minor releases ship coherent, owner-gated feature sets and
may include pre-1.0 API refinements. See [`docs/roadmap.md`](docs/roadmap.md)
for the per-release rationale and Definitions of Done.

## [Unreleased]

## [0.26.0] — 2026-06-25

*Elastic* — the cluster grows and replicates: per-shard read replicas (increment 2)
and dynamic, elastic membership with online rebalancing behind a coordinator
(increment 3 — ADR-0066), all opt-in, with single-node unchanged at zero overhead.

### Added

- **Cluster online slice migration — data plane** (ADR-0066, increment 3c). When a
  shard joins, the coordinator can add it in a **joining** state (`POST
  /cluster/shards/joining`) and later **promote** it (`POST
  /cluster/shards/{id}/promote`). While a shard is joining, the router **dual-writes**
  the migrating slice to both the joining owner and the **donor** that still serves it,
  serves searches from the active shards (excluding the joining one, whose donor holds
  the authoritative slice), and routes gets to the donor — so the slice stays queryable
  and no write is lost. At promotion the slice's ownership flips atomically (a version
  bump); a **dedup-by-id gather** absorbs the brief window where the donor and the
  promoted shard both hold a slice point. `quiver_cluster::ShardMap` gains a `joining`
  set with `add_joining_shard`/`promote`/`donor_for`/`active_shards`/
  `partition_to_donors`. An end-to-end test drives a join→copy→flip migration and
  proves the slice is queryable throughout and every acknowledged write survives the
  flip.
- **Automated cluster growth** (ADR-0066, increment 3c-ii). `POST /cluster/shards/grow`
  adds a shard and runs the **whole online migration in the background** — wait for
  routers to adopt the joining map (dual-write live), **copy** the new shard's slice
  from the donors (paginated scroll, **get-if-absent** so a concurrent dual-write is
  never clobbered, provisioning the collection schema on the new shard), **promote**
  (flip), then **drop** the donors' now-stale copies — so an operator grows the cluster
  with a single call and the slice stays queryable with no lost writes throughout. On
  any failure the join is reverted. (Single-vector collections; a multivector
  collection aborts the migration honestly rather than dropping its slice silently.)
  The point `fetch` endpoint gains an `offset` for paginated scroll. Single-node and a
  static cluster are unaffected.
- **Cluster coordinator + dynamic router refresh** (ADR-0066, increment 3b). A new
  opt-in **coordinator** mode (`QUIVER_COORDINATOR=true`) runs a thin, data-plane-free
  service that owns the authoritative, monotonically **versioned** shard map and serves
  a small membership API — `GET /cluster/map`, `POST /cluster/shards`,
  `DELETE /cluster/shards/{id}`, `GET /cluster/health` — persisting its map + a
  never-reused id counter to `QUIVER_COORDINATOR_STATE` so a restart recovers exactly.
  A **router** given `QUIVER_COORDINATOR_URL` refreshes its shard map from the
  coordinator on an interval and swaps any newer version into its `ArcSwap`, so adding
  or removing a shard propagates **with no restart**; the coordinator is never a
  per-query dependency. A router also exposes its currently adopted map read-only at
  `GET /cluster/map`. No data migration yet (that is 3c) — a freshly added shard owns
  only the keys that now hash to it. Single-node and a static cluster are unaffected.

### Changed

- **Stable shard ids** (ADR-0066, increment-3 groundwork). `quiver_cluster::Shard`
  now carries an immutable `id: u64` — **the HRW key** — instead of a positional
  `index: usize`, decoupled from the shard's position so a shard can be removed
  without re-keying (and moving the data of) its survivors. `ShardMap` keys
  `partition`/`add_replica` by id and gains `from_shards` for non-contiguous ids (the
  gap a removed shard leaves); `partition` now yields `(&Shard, group)`. `from_urls`
  is unchanged (ids `0..N`), so a static cluster's routing and on-the-wire behaviour
  are byte-identical and `QUIVER_CLUSTER_REPLICAS`'s `<shard_id>=<url>` form still
  targets `0..N`. Pure `quiver-cluster` refactor; single-node unaffected.

### Added

- **Cluster read replicas** (ADR-0065 increment 2), opt-in via
  `QUIVER_CLUSTER_REPLICAS` (a list of `<shard_index>=<replica_url>` entries). Each
  shard can now declare one or more **read replicas** — ordinary leader-follower
  followers (ADR-0030) of the shard's primary, reused unchanged. Writes, gets and
  deletes still go to the single primary per shard; **searches round-robin across
  `{primary} ∪ replicas`** to spread read load, falling back to another copy if one
  is unreachable. Replicas are eventually consistent (replication lag) and refuse
  direct writes, so a mis-route cannot corrupt one. `quiver_cluster::Shard` gains
  `replica_urls` + `read_url`/`read_order`; `ShardMap::add_replica` attaches them.
  An end-to-end test boots primaries + followers + a router + a single-node baseline
  and proves replica-served top-k equals the baseline, a replica refuses writes, and
  the router tolerates a down replica. Single-node and primary-only shards are
  unaffected. The **"Quiver, Explained"** field guide gains §9.9 + a read-replica
  figure (now 54 pages).

### Fixed

- `QUIVER_CLUSTER_SHARDS` / `QUIVER_CLUSTER_REPLICAS` env values are figment array
  literals and must be **bracketed** (`[http://s1:6333,http://s2:6333]`); the
  `.env.example` and config docs previously showed an unbracketed comma list, which
  figment rejects as "expected a sequence" at startup.

## [0.25.0] — 2026-06-25

### Added

- **Cluster mode — sharding + scatter-gather** (ADR-0065 increment 1), opt-in via
  `QUIVER_CLUSTER_SHARDS`. A non-empty list of shard URLs makes the server a
  stateless **router**: it shards writes by point id using **rendezvous (HRW)
  hashing** — so adding or removing a shard remaps only ~1/N of ids, the basis for
  the **dynamic, elastic scaling** the cluster is designed for — and **scatter-gathers**
  searches across all shards, merging the exact global top-k. Each shard is an
  ordinary single-writer Quiver engine with its own `kill -9` crash gate; the
  cluster is composition, not a rewrite. A new `quiver-cluster` crate holds the pure
  primitives (shard map, HRW hashing, merge). An end-to-end test proves a two-shard
  router returns the same top-k distances as a single-node baseline holding the same
  data, and that writes shard and routed get/delete work. Single-node stays the
  zero-overhead default (the router is `None` unless configured). Later increments:
  read replicas, online elastic membership + rebalancing + a coordinator, per-shard
  Raft write-HA, autoscaling hooks.

## [0.24.0] — 2026-06-25

### Added

- Lock-free MVCC reads (ADR-0064), **experimental and default-off** behind
  `QUIVER_MVCC_READS`. For single-vector, in-memory collections the single writer
  publishes an immutable `CollectionSnapshot` (the base index plus a small overlay
  of writes since the last rebuild) into an `arc-swap` cell; a reader `load()`s it
  and merges base ⊕ overlay **without taking any lock**, so reads no longer block
  on a concurrent writer's exclusive lock. Reads served from the snapshot now cover
  **pure-vector, payload/vector, filtered (exact pre-filter and post-filter), and
  hybrid (dense ⊕ sparse/BM25)** — reusing the same store-fetch and RRF logic as
  the locked path. Durability and the `kill -9` crash gate are unchanged — MVCC
  changes visibility, not durability. Justified by a measured read-during-write
  contention sweep (`docs/benchmarks/results/read-during-write.md`): a single
  concurrent writer of small upserts already retains only ~0.10× of read throughput
  under the `RwLock`. At the server, an MVCC-served collection's snapshot cell is
  cached **outside** the database lock, so a **pure-vector** query loads it and
  searches with no lock at all — it never blocks on a concurrent writer (payload/
  filtered/hybrid reads keep the read lock for the store fetch, which is not safe
  lock-free under a writer, but also serve from the snapshot). Enable with
  `mvcc_reads = true` (config) or `QUIVER_MVCC_READS=1`. A before/after sweep on the
  same box (`docs/benchmarks/results/read-during-write.md`) confirms the win: under
  two small-upsert writers, retained read-QPS goes from **0.00× (RwLock) to 0.79×
  (MVCC)**, and from ~0.01× to ~0.67× under four. The flag stays **default-off**
  (the proven `RwLock` path remains the default) until validated on dedicated
  hardware — absolute QPS is `reference-hardware-pending`, only the ratio is the
  honest signal on a shared dev box.
- Read-during-write contention sweep now measures a grid of write pressure
  (writer-thread counts × upsert batch sizes), recording the retained-read-QPS
  ceiling that gates the MVCC build.

## [0.23.0] — 2026-06-24

### Added

- Durable on-disk DiskVamana index (ADR-0063): a `disk_vamana` collection now
  **loads its frugal `mmap` base on open** — `mmap` the immutable base graph,
  reconstruct the small in-memory FreshDiskANN delta from the store, and replay
  the post-checkpoint WAL tail — instead of an `O(N)` full-RAM rebuild on every
  restart. The base file is published by atomic rename and a tiny checkpoint blob
  (base count, tombstones, id map) ties it to the live state; the delta vectors
  are refetched from the store by id, so the blob stays O(delta ids), not O(N).
  Any problem falls back to the authoritative rebuild, so the artifact is never
  load-bearing for correctness and the `kill -9` crash gate is preserved by
  construction. This is what finally lets a running server serve from the
  PQ-codes-resident path after a restart — the memory-frugality wedge — where
  earlier releases rebuilt from every full-precision vector on open (the cause of
  the benchmark's unrepresentative post-build RSS).
- Cold-reopen-honest memory-frugality evidence: the disk-path wedge benchmark
  now **closes and cold-reopens the server before sampling RSS** (#279), so the
  reported serving memory reflects the durable load path rather than a warm
  post-build process; a **one-command Windows disk-path frugality runner**
  (`scripts/bench-disk-frugality.ps1`, #275) builds, cold-reopens, and samples
  on a clean box; and a **read-during-write contention sweep**
  (`docs/benchmarks/results/read-during-write.md`, #281) measures the read-QPS
  retained under one concurrent writer — the measure-first gate for MVCC.
  Absolute serving-RAM and QPS stay reference-hardware-pending, never fabricated.
- Lock-free MVCC reads implementation design (ADR-0064): resolves the tension
  ADR-0053 left open (indexes are mutated in place, so they cannot simply be
  shared by `Arc`) with a per-collection arc-swap snapshot plus a small
  copy-on-write overlay, published on commit — staged in three increments behind
  a default-off `QUIVER_MVCC_READS` flag and gated on the measured contention
  above. Design only in this release; the read path is unchanged.

### Changed

- `QUIVER_DISABLE_DURABLE_DISK_INDEX` ops kill switch forces the (always-correct)
  rebuild-on-open path if the durable load is ever suspected. Durable load is on
  by default.

### Security

- Addressed code-scanning findings (#282): the TypeScript SDK's unsubscribe-link
  regex is rewritten to remove a ReDoS vector, and the DCPE "hard-coded
  cryptographic value" alerts are documented as false-positive/by-design at
  `crates/quiver-crypto/src/dcpe.rs` — they are zeroed buffers filled by `OsRng`,
  a tag output buffer, a test constant, and one deliberate key-derived
  deterministic IV, not embedded secrets.

### Fixed

- Bumped vulnerable dependencies flagged by Dependabot (#283): `pydantic-settings`,
  `langsmith`, and the `trivy-action` CI pin (dev/bench/CI dependencies only; the
  engine and SDKs are unaffected).

## [0.22.0] — 2026-06-24

### Added

- Benchmark dimensions (ADR-0061): the v0.22.0 SIFT1M run reports **recall at
  depth 1 / 10 / 100**, a **saturated-concurrency** sweep (1-thread vs 8-thread
  QPS — up to 1.76× at ef=256), a **quantization trade-off** (in-memory HNSW vs
  disk-Vamana + PQ, showing the recall@100 tail collapse), and a **filtered
  selectivity** sweep (the planner's pre-filter/post-filter recall valley). Every
  figure traces to a committed CSV in `docs/benchmarks/results/comparison-v0.22.0/`;
  absolute serving-RAM and full-field saturated QPS stay reference-hardware-pending,
  never fabricated.
- The "Quiver, Explained" field guide is expanded to v0.22.0 with a whole-system
  architecture diagram, the off-lock-rebuild timeline, the new benchmark figures,
  and a cockpit walkthrough; its standalone figures are now committed under
  `docs/assets/explained-figures/`.
- Interactive TUI cockpit (ADR-0060): the retro cockpit gains a **query runner**
  (`/`) — type a query, run a server-side embed-and-search, inspect any result's
  payload, and recall recent searches — plus a modal keybinding-help overlay
  (`?` / `F1`), a live theme toggle (`Ctrl-t`, Bronze ↔ Slate), and an
  ingest-rate sparkline alongside the points trend. Key handling is refactored
  into a pure, table-tested dispatcher with network I/O pushed to the edge; every
  screen renders to a buffer and is asserted with ratatui's `TestBackend`. New
  committed screenshots (`search`, `help`, `theme-slate`) regenerate from the
  real render via `just tui-shots`.
- OpenTelemetry traces exporter (ADR-0059): opt-in behind the `otlp` cargo
  feature and a runtime endpoint (`QUIVER_OTLP_ENDPOINT` / `[otlp]` in
  `quiver.toml`), exporting the existing `#[tracing::instrument]` spans over
  OTLP/gRPC to a collector (Jaeger/Tempo/Grafana). Off by default — no new
  dependencies in a normal build; the OTLP/gRPC transport reuses the in-tree
  `tonic`. A failed exporter degrades to `fmt`-only rather than failing startup.
- MCP text tools (ADR-0058): `upsert_text` and `search_text` over the MCP server,
  so an AI agent can store and search documents by text — Quiver embeds them
  server-side — without running an embedding model itself. Configured with
  `quiver mcp --config <quiver.toml>` (`[embedding.<collection>]` /
  `[rerank.<collection>]` tables, the same surface as `quiver serve`); `search_text`
  optionally reranks. This brings the MCP surface to full provider parity with
  REST, gRPC, and the SDKs.

### Changed

- Index rebuilds run **off the exclusive lock** (ADR-0062): a write that defers a
  collection's rebuild no longer stalls concurrent reads. The server serves the
  prior snapshot while it rebuilds the index off-lock — captured under the shared
  read lock, built with no lock held, swapped in under a brief write lock — so a
  rebuild that previously blocked every read for the whole build (measured ~8 s at
  20k vectors, ~30 s at 50k, ~77 s at 100k) now keeps reads in the sub-millisecond
  tail. Server reads are snapshot-isolated and eventually consistent across a
  rebuild window; embedded `&mut` searches still rebuild synchronously for
  read-your-writes. Durability and the crash-recovery gate are unchanged.
- The embedding/rerank provider seam moved from `quiver-server` into a new lean
  `quiver-providers` crate (ADR-0058) shared by the network and MCP servers; the
  server re-exports the types, so its public API is unchanged.
- Crates are now published under the `quiverdb-*` namespace (ADR-0056): each
  package is renamed `quiverdb-<crate>` while its library/extern name stays
  `quiver_<crate>` and the binary stays `quiver`, so source, imports, and
  `cargo install --path` are unchanged. This unblocks the (owner-gated) crates.io
  publish job, since `quiver-core` / `quiver-cli` are held by unrelated crates.

## [0.21.0] — 2026-06-23

### Added

- Packaging & distribution pipeline (ADR-0056): backfilled `CHANGELOG.md`, crate
  publish metadata, secret-gated crates.io / PyPI / npm publish jobs, and a Helm
  chart + Kubernetes manifests under `infra/`.
- TypeScript SDK parity with the Python async client: `upsertIter` (batches a
  sync or async iterable), `scroll` (an async generator for export / re-embedding),
  and `deleteByFilter` (paged erasure).
- Go SDK bulk/maintenance helpers: `UpsertBatch` (batched upload), `Scroll` (page
  through a collection via a callback), and `DeleteByFilter` (paged erasure) — all
  context-aware, standard-library only.

### Changed

- Concurrent reads (ADR-0057): the server serves searches behind a reader–writer
  lock instead of a single mutex, so reads run in parallel. The engine gains
  `&self` `search_snapshot` / `hybrid_search_snapshot` /
  `search_multi_vector_snapshot` reads and `ensure_indexed`; the single-writer
  model, durability, and crash gate are unchanged. Fully lock-free arc-swap
  snapshots are the staged successor.

### Fixed

- Go SDK `Fetch` parsed the wrong response envelope (`matches` instead of the
  `points` the fetch endpoint returns), so it never returned points; now fixed,
  with a regression test.

## [0.20.1] — 2026-06-23

### Changed

- Re-ran the multi-DB benchmark on the v0.20.0 engine with the bulk-ingest build
  path so the build column is an honest *time-until-queryable* (ADR-0055); folded
  SIFT1M + GIST1M results across seven competitors into the README, docs, and the
  "Quiver, Explained" field guide.

### Fixed

- "Quiver, Explained" PDF layout: figure overlaps and the ColBERT callout page break.

## [0.20.0] — 2026-06-23

### Added

- Online snapshot & restore — consistent whole-directory copy over REST and the
  MCP server (ADR-0050).
- Client-streaming gRPC `UpsertStream` (ADR-0045 fast-follow).
- Real Prometheus `/metrics` (counters + latency histogram + security counters),
  request tracing spans, and an importable Grafana dashboard (ADR-0054).
- Per-key rate limiting — token bucket with `RateLimit-*` headers and `429`
  (ADR-0049).
- Server-side embedding & reranking hooks, provider-agnostic and opt-in per
  collection (ADR-0047); BM25 with a Snowball (Porter2) stemmer (ADR-0048).
- A standard-library Go SDK mirroring the REST surface; `snapshot()` parity in
  the Python and TypeScript SDKs.
- Design-only ADRs for the big bets: distributed/sharded mode (0051), GPU
  acceleration (0052), and lock-free MVCC reads (0053).

## [0.19.0] — 2026-06-22

### Added

- Hybrid (dense + sparse) search on every surface — gRPC, MCP, and the
  TypeScript SDK join REST and Python — backed by a derived sparse inverted
  index; bulk ingest via `POST …/points:bulk` (ADR-0045).

### Fixed

- De-noised CI by isolating and retrying the virtualization-sensitive
  crash-recovery test.

## [0.18.1] — 2026-06-19

### Fixed

- Automated, tag-triggered Windows release asset so `quiver update` resolves on
  Windows; unified multi-platform release packaging (ADR-0044).

## [0.18.0] — 2026-06-19

### Added

- Query cost limits enforced at the op layer (caps on `k`, `ef_search`,
  dimension, payload, batch), closing an authenticated-DoS vector (ADR-0040).
- Deep, large-data benchmark dimensions with real SIFT1M and GIST1M multi-DB
  comparisons and a Docker Milvus-server adapter (ADR-0041).
- RAG/agentic ergonomics — async Python client, batched upsert / scroll /
  delete-by-filter, a Haystack `DocumentStore`, a `rerank` helper, and an MCP
  `collection_info` tool (ADR-0042).
- Hybrid (dense + sparse) search with RRF fusion over the engine, REST, and the
  Python SDK (ADR-0043).

## [0.17.2] — 2026-06-19

### Fixed

- Windows install/update hotfix (`fsync_dir` behavior) and CDN stale-asset cache bypass.

## [0.17.1] — 2026-06-19

### Changed

- Verdigris "V" arrowhead in all terminal banners matching the TUI logo;
  regenerated cockpit screenshots.

## [0.17.0] — 2026-06-18

### Added

- One-command install (`install.sh` / `install.ps1`) and a self-updating
  `quiver update` subcommand with checksum verification (ADR-0039).
- Scientific multi-DB benchmark suite (ADR-0037).

### Changed

- 35× faster upsert build time via batched WAL sync (ADR-0038).

## [0.16.0] — 2026-06-18

### Added

- The retro cockpit (ADR-0036): a coherent Bronze Quiver brand, a logo whose "V"
  is a 3-D arrowhead, a vocabulary of retro decorations, a render-to-buffer view
  API, and reproducible committed screenshots (`just tui-shots`).

## [0.15.0] — 2026-06-17

### Added

- mdBook documentation site under `apps/docs`; a native TypeScript `DcpeCipher`
  closing the last DCPE SDK gap (ADR-0035).

### Changed

- DCPE Scale-And-Perturb hardening as a breaking cipher v2 — key-derived
  component shuffle (an exact L2 isometry) plus an optional ordering-preserving
  affine normalisation; full per-axis whitening is documented as incompatible
  with searchable encryption and deliberately not offered (ADR-0035).

## [0.14.0] — 2026-06-17

### Added

- Multi-vector / ColBERT follow-ups (ADR-0034): incremental multi-vector index
  maintenance and an opt-in ColBERTv2 residual-compression index with PLAID
  centroid pruning, creatable across REST/gRPC/MCP and the SDKs. Native
  variable-stride document rows were deferred honestly, gated on a measured
  locality win.

## [0.13.0] — 2026-06-17

### Added

- Graph-index incremental updates — Vamana and DiskVamana adopt FreshDiskANN's
  StreamingMerge model (in-memory delta + `O(1)` tombstones + churn-threshold
  consolidation), the last index family that previously rebuilt on every write
  (ADR-0033).

## [0.12.0] — 2026-06-16

### Fixed

- Install honesty (the `quiver-cli` crates.io name is an unrelated crate → install
  from source), static README badges that render everywhere, and UTF-8 mojibake in
  the status banner. No functional change.

## [0.11.0] — 2026-06-16

### Added

- Semantically secure client-side vector encryption — `vector_encryption =
  client_side` seals each vector with XChaCha20-Poly1305 and stores an opaque
  blob, the server does no ranking, and retrieval is a client-side fetch-and-rank
  (ADR-0032). The flag migrated from `encrypted_vectors` to a three-valued
  `vector_encryption` enum (byte-compatible on disk).

## [0.10.0] — 2026-06-15

### Added

- Experimental DCPE vector-encryption mode — the published Scale-And-Perturb
  distance-comparison-preserving scheme, opt-in and off by default, with honest
  limits (L2-only, not semantically secure) (ADR-0031).

## [0.9.0] — 2026-06-15

### Added

- Asynchronous leader-follower replication — a follower applies the leader's
  committed op log and serves reads while refusing writes; failover and consensus
  are explicit non-goals (ADR-0030).

## [0.8.0] — 2026-06-15

### Added

- Live Chroma and Postgres migration connectors, completing one-command live
  migration from Qdrant, Chroma, and pgvector (ADR-0029).

## [0.7.0] — 2026-06-15

### Added

- Multi-vector / late-interaction (ColBERT) retrieval — `multivector` collections
  ranked by MaxSim over the existing row-addressed store, reachable from every
  surface (ADR-0028).

## [0.6.0] — 2026-06-15

### Added

- Durable on-disk incremental IVF index recovered by snapshot + WAL-tail replay
  under the manifest's atomic swap (ADR-0025); a live Qdrant migration connector
  over HTTP (ADR-0027).

## [0.5.0] — 2026-06-15

### Added

- HNSW incremental delete (`O(1)` soft-delete + amortized rebuild, ADR-0026);
  neighbor-bounded IVF reassignment (ADR-0023); a unified secure database-open
  path shared by the server, MCP, and CLI.

## [0.4.0] — 2026-06-14

### Added

- Incremental in-place IVF index updates (SpFresh/LIRE, in-memory so the crash
  gate is untouched, ADR-0023); migration importers for Qdrant / Chroma / pgvector
  exports (ADR-0024).

## [0.3.0] — 2026-06-14

### Added

- Security depth — client-side payload encryption with a documented trust
  boundary, RBAC + scoped API keys + optional mTLS, an append-only audit log,
  per-collection-DEK envelope encryption with crypto-shredding, and the cockpit
  constellation view; `cargo-fuzz` targets for the wire and on-disk parsers.

## [0.2.0] — 2026-06-14

### Added

- Memory frugality — disk-resident graph (DiskANN/Vamana) + IVF, quantization
  (PQ / scalar / binary with hamming pre-filter + exact re-rank), per-collection
  recall/latency/memory knobs, hybrid filtered search, the TypeScript SDK, the MCP
  server, and the LangChain / LlamaIndex adapters.

## [0.1.0] — 2026-06-13

### Added

- Single-node core — encrypted collection → upsert → filtered k-NN → live TUI from
  one binary; storage engine (segments + WAL + crash recovery + checksums); HNSW;
  SIMD kernels; REST + gRPC; encryption-at-rest by default; TLS via `rustls`; the
  TUI MVP; the benchmark harness with first SIFT1M numbers; the Python SDK.

[Unreleased]: https://github.com/achref-soua/quiver/compare/v0.26.0...HEAD
[0.26.0]: https://github.com/achref-soua/quiver/compare/v0.25.0...v0.26.0
[0.25.0]: https://github.com/achref-soua/quiver/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/achref-soua/quiver/compare/v0.23.0...v0.24.0
[0.22.0]: https://github.com/achref-soua/quiver/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/achref-soua/quiver/compare/v0.20.1...v0.21.0
[0.20.1]: https://github.com/achref-soua/quiver/compare/v0.20.0...v0.20.1
[0.20.0]: https://github.com/achref-soua/quiver/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/achref-soua/quiver/compare/v0.18.1...v0.19.0
[0.18.1]: https://github.com/achref-soua/quiver/compare/v0.18.0...v0.18.1
[0.18.0]: https://github.com/achref-soua/quiver/compare/v0.17.2...v0.18.0
[0.17.2]: https://github.com/achref-soua/quiver/compare/v0.17.1...v0.17.2
[0.17.1]: https://github.com/achref-soua/quiver/compare/v0.17.0...v0.17.1
[0.17.0]: https://github.com/achref-soua/quiver/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/achref-soua/quiver/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/achref-soua/quiver/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/achref-soua/quiver/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/achref-soua/quiver/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/achref-soua/quiver/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/achref-soua/quiver/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/achref-soua/quiver/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/achref-soua/quiver/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/achref-soua/quiver/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/achref-soua/quiver/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/achref-soua/quiver/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/achref-soua/quiver/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/achref-soua/quiver/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/achref-soua/quiver/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/achref-soua/quiver/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/achref-soua/quiver/releases/tag/v0.1.0
