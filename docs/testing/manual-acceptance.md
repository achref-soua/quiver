# Acceptance checklist — test every surface like a real operator

This is the human-facing acceptance checklist for a release (the v0.17.0
launch-hardening pass, Phase A). It enumerates everything Quiver does, across
every external surface and every index / quantization / encryption mode, and maps
each item to the automated coverage that proves it — plus the manual steps for what
genuinely still needs a human (chiefly real-server migration imports against a live
Qdrant/Chroma/Postgres).

The interactive cockpit **used to be on that list and no longer is.** A pty is a
terminal, which is all `asciinema` and `crossterm` require, so
[`scripts/record-cockpit-cast.sh`](../../scripts/record-cockpit-cast.sh) drives a
scripted tour unattended and [`docs/assets/cockpit.cast`](../assets/cockpit.cast)
regenerates in one command. A human tour is still the better *demo*; it is no longer
the only way to get one.

> **Honesty.** This is a *correctness* acceptance pass. Performance numbers come
> only from the documented reference hardware
> ([`docs/benchmarks/`](../benchmarks/methodology.md)) and are never produced from
> this checklist.

## How to run the automated pass

```bash
just verify          # the full gate: fmt · clippy -D · cargo test --workspace · doc · deny · audit
just test-py         # Python SDK suite (HTTP mocked)
just test-ts         # TypeScript SDK suite (fetch mocked)
just acceptance      # boots a real encrypted server, drives REST + both SDKs + CLI + MCP
```

`just acceptance` ([`scripts/acceptance.sh`](../../scripts/acceptance.sh) +
[`scripts/acceptance_sdk.py`](../../scripts/acceptance_sdk.py) +
[`scripts/acceptance_sdk.mjs`](../../scripts/acceptance_sdk.mjs)) is the live
end-to-end run: it starts a server with **encryption-at-rest ON** on loopback alt
ports and exercises the lifecycle (create → upsert → filtered search → get →
delete → drop) across every surface and mode below.

**It also runs in CI** (the `acceptance` job), so these surfaces are gated on every
pull request rather than only before a release. That matters most for the SDKs: both
suites (`just test-py`, `just test-ts`) mock the transport entirely, so they prove the
client's shape but never that the shape matches what the server sends — a renamed
field or a reclassified status code passes under mocks and fails in a user's hands.
The acceptance run is what crosses a real socket.

For the deeper deterministic guarantees, see also:

- **Proptests** — `wal::entries_roundtrip`, `wal::truncation_yields_a_clean_prefix`,
  `page::any_body_roundtrips` (raise the case count with `PROPTEST_CASES=8192`).
- **Fuzzers** — `just fuzz filter_json|page_decode|wal_decode` (see
  [`docs/security/fuzzing.md`](../security/fuzzing.md)).
- **Crash recovery** — `cargo test -p quiverdb-core --test crash_recovery`
  (run isolated; it flakes under the parallel gate).

## Surfaces × operations

Legend: ✅ automated (named test / script) · 🖐 manual step.

### Embeddable database (`quiver-embed::Database`)

| Operation | Coverage |
| --- | --- |
| open / open-with-keyring (encrypted at rest) | ✅ `quiver-cli` `admin.rs` import tests; `crash_recovery.rs` |
| create / drop collection (crypto-shred on drop) | ✅ `crates/quiver-crypto/tests/envelope_shred.rs` |
| upsert / get / delete points | ✅ `quiver-embed` unit tests; `scripts/acceptance_sdk.py` (via REST) |
| filtered (hybrid) search | ✅ `acceptance_sdk.py` per index kind |
| crash-safe restart mid-write | ✅ `crates/quiver-core/tests/crash_recovery.rs` |

### REST server

| Operation | Coverage |
| --- | --- |
| health / readiness | ✅ `acceptance.sh` (`/readyz`) |
| create → upsert → query → get → delete → drop | ✅ `crates/quiver-server/tests/round_trip.rs`; `acceptance.sh` (curl) |
| hybrid (pre-filtered) search | ✅ `round_trip.rs`; `acceptance.sh` |
| RBAC: wrong key / missing key denied (401) | ✅ `crates/quiver-server/tests/rbac.rs`; `acceptance.sh` |
| TLS / mTLS | ✅ `crates/quiver-server/tests/tls.rs` |
| audit log records mutations + denials, no secrets | ✅ `crates/quiver-server/tests/audit.rs` |

### gRPC server

| Operation | Coverage |
| --- | --- |
| full CRUD + query parity with REST | ✅ `round_trip.rs` (gRPC arm), `dcpe.rs`, `client_side_vectors.rs`, `multivector.rs` |
| RBAC denials | ✅ `rbac.rs` |
| TLS | ✅ `tls.rs` |

### MCP server (JSON-RPC over stdio)

| Operation | Coverage |
| --- | --- |
| `initialize` / `tools/list` | ✅ `crates/quiver-mcp/src/lib.rs` tests; `acceptance.sh` |
| `create_collection` / `upsert` / `search` / `fetch` / `get` / `delete` | ✅ mcp unit tests; `acceptance.sh` |
| `upsert_document` / `search_multi_vector` / `delete_document` | ✅ mcp `multivector_tools_*` test |

### Python SDK

| Operation | Coverage |
| --- | --- |
| client lifecycle, search, fetch | ✅ `sdks/python/tests/test_client.py`; `acceptance_sdk.py` (live) |
| DCPE / client-side ciphers | ✅ `test_dcpe.py`, cross-lang KAT; `acceptance_sdk.py` (live) |
| multi-vector documents | ✅ `acceptance_sdk.py` (live) |
| LangChain / LlamaIndex adapters | ✅ `test_langchain.py`, `test_llamaindex.py` |

### TypeScript SDK

| Operation | Coverage |
| --- | --- |
| client lifecycle, search, fetch (fetch mocked) | ✅ `sdks/typescript/test/*` via `just test-ts` |
| native DCPE / vector ciphers + cross-lang KAT | ✅ `test/dcpe.test.ts`, `test/vector.test.ts` |
| **wire contract against a live server** | ✅ `scripts/acceptance_sdk.mjs` via `acceptance.sh` |

### CLI (`quiver`)

| Subcommand | Coverage |
| --- | --- |
| `serve` | ✅ booted by `acceptance.sh` |
| `mcp` | ✅ `acceptance.sh` |
| `admin import` (offline, encrypted at rest) | ✅ `admin.rs` tests; `acceptance.sh` |
| `admin import` cleartext-credential warning | ✅ `live.rs` tests; `acceptance.sh` |
| `tui` | ✅ scripted tour via `scripts/record-cockpit-cast.sh` (pty-driven, asserts it reached both views); render path ✅ `crates/quiver-tui/tests/live.rs`; 🖐 a human tour is still worth doing before a launch |

### TUI cockpit

The cockpit is an interactive terminal app; its render is asserted headless
(`crates/quiver-tui` buffer tests + `tests/live.rs` against a real server), and
the screenshots are regenerated reproducibly with `just tui-shots`. Manual smoke:

- 🖐 `just demo` in one terminal, then
  `quiver tui --url http://127.0.0.1:6333 --api-key quiver-demo-key` in another.
- 🖐 Confirm: the bronze logo renders; the dashboard shows the ONLINE badge, the
  points-trend sparkline, the relationships tree, the collections table with load
  bars, and the activity log; pressing `c` opens the constellation view and `Esc`
  returns; stopping the server flips the badge to a rust-red OFFLINE.

## Index kinds × quantization (correctness)

Quiver's quantization is **product quantization (PQ)**, selected at the collection
API via `pq_subspaces` for the disk graph and IVF, and used as residual-PQ inside
the ColBERT token-pool index. The `ScalarQuantizer` and `BinaryQuantizer` are
**internal index primitives** (unit-tested in `crates/quiver-index/src/quant/`,
e.g. `scalar_quantizer_recall_with_rerank`, `binary_quantizer_prefilter_then_rerank`)
— they are not separately selectable when creating a collection, so the live
acceptance run covers PQ where the index supports it.

| Index kind | Live acceptance (`acceptance_sdk.py`) | Unit/integration |
| --- | --- | --- |
| `hnsw` | ✅ cosine, full lifecycle | ✅ `quiver-index` hnsw tests |
| `ivf` (+ PQ) | ✅ l2, `pq_subspaces=4` | ✅ `quiver-index` ivf tests |
| `vamana` | ✅ l2 | ✅ `quiver-index` vamana + `fresh` tests |
| `disk_vamana` (+ PQ) | ✅ l2, `pq_subspaces=4` | ✅ `quiver-index` disk tests |
| `colbert` (multivector, residual-PQ) | ✅ cosine, documents | ✅ `colbert.rs` + `multivector.rs` |

## Encryption modes

| Mode | What it protects | Live acceptance | Integration |
| --- | --- | --- | --- |
| Encryption-at-rest (always on unless `--insecure`) | data files are ciphertext | ✅ server booted with a key | ✅ `at_rest.rs`, `envelope_shred.rs` |
| `vector_encryption=dcpe` | property-preserving, server ranks ciphertexts (not IND-CPA) | ✅ encrypt + ranked query | ✅ `dcpe.rs` |
| `vector_encryption=client_side` | opaque AEAD, server cannot rank | ✅ seal + fetch + local rank + server refuses ranking | ✅ `client_side_vectors.rs` |

## Migration importers

| Source | Offline (file) | Live (running source) |
| --- | --- | --- |
| Qdrant | ✅ `admin.rs` + `acceptance.sh` | ✅ `live.rs` (hermetic HTTP); 🖐 real server |
| Chroma | ✅ `lib.rs` parse tests | ✅ `live.rs` (hermetic HTTP); 🖐 real server |
| pgvector | ✅ `lib.rs` parse tests | ✅ `live.rs` connection-error path; 🖐 `cargo test -p quiverdb-import -- --ignored` with `QUIVER_PG_TEST_URL` |

## Replication

| Behavior | Coverage |
| --- | --- |
| leader → follower catch-up | ✅ `crates/quiver-embed/tests/replication.rs`, `crates/quiver-server/tests/replication.rs` |
| follower refuses writes | ✅ `replication.rs` |

## Sign-off

A release is acceptance-clean when: `just verify`, `just test-py`, `just test-ts`,
and `just acceptance` are green; the proptests, fuzzers, and `crash_recovery` are
green; and the two 🖐 manual items (the cockpit smoke and, if migrating, a real
Qdrant/Chroma/pgvector import) have been eyeballed. Record the run in the release
notes; never claim a surface verified that was not actually exercised.
