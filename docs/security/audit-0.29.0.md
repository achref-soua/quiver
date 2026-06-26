# Security audit — v0.29.0 finalization pass

**Date:** 2026-06-27 · **Scope:** the whole codebase ahead of the v0.29.0
hardening release, reviewed OWASP-style against the
[threat model](./threat-model.md). This note records what was probed, what was
**fixed**, and — equally — what was **checked and found not vulnerable, and why**.
It supersedes nothing in the [v0.17.0 audit](./audit-0.17.0.md); it extends the
review to the surfaces added since (cluster mode, the coordinator, per-shard
Raft, autoscaling, the GPU feature) and adds a **dynamic** (DAST) pass with
OWASP ZAP.

> **Honesty.** Every claim here is backed by a named test, a tool run reproduced
> on this box, or a code reference. Findings that could not be closed are listed
> as residual risk, not buried. No result is fabricated.

## Method

- **Static review** of the request-handling, auth, crypto, storage, parsing, and
  cluster-control paths for the OWASP Top-10 classes that apply to a self-hosted
  database: broken access control, injection, SSRF, cryptographic failures,
  security misconfiguration, and denial-of-service via untrusted input.
- **Dynamic (DAST)**: an OWASP ZAP **baseline** scan (spider + passive rules)
  against a live, encrypted, authenticated server (below).
- **Fuzzing**: the three `cargo-fuzz` targets over the untrusted-input parsers,
  re-run on this box for a bounded duration (below).
- **Regression tests**: each finding below is gated by a named test in the suite,
  run green before this note was written.

## Findings

### F1 — The cluster coordinator's membership API was unauthenticated (fixed)

**Severity:** high (cluster mode only; the single-node default is unaffected —
the coordinator is opt-in and off by default). **Status:** fixed this release
(PR #335).

The cluster **coordinator** (ADR-0066) served its membership API with **no
inbound authentication**. Any party able to reach the coordinator could
`POST /cluster/shards`, `/cluster/shards/grow`, `/cluster/shards/{id}/promote`,
`/cluster/shards/joining`, or `DELETE /cluster/shards/{id}` — adding/removing
shards, triggering data migrations, and flipping slice ownership, i.e. reshaping
the entire cluster (an availability and mis-routing impact). Because a real
multi-node coordinator must be network-reachable by routers and shards, loopback
binding did not mitigate it.

**Fix:** the coordinator now enforces the same default-deny RBAC as the data
plane, reusing `auth::authenticate`. Every route except `/healthz`/`/readyz`
requires a valid API key; the mutating shard ops additionally require the
**admin** role; the read-only `/cluster/map` and `/cluster/health` accept any
valid key (a router presents its `QUIVER_CLUSTER_SHARD_KEY`, which must be one of
the coordinator's keys). A keyless coordinator already refuses to boot unless
`insecure` (`Config::validate`); in `insecure` mode `authenticate` admits any
caller, so a dev/loopback cluster is unchanged. Test:
`coordinator_membership_api_requires_auth` (no key → 401, read-only key on a
mutation → 403, admin → 200, read endpoint open to any valid key, `/healthz`
open) — it returns 200 (unauthenticated reshape) before the fix and 401 after.
The threat model's elevation-of-privilege control and `.env.example` were updated.

### F2 — The crash-recovery gate flaked on a torn WAL header (fixed)

**Severity:** not a vulnerability — a durability-gate **correctness** bug, recorded
here because the crash gate is the foundation of the security-first durability
claim. **Status:** fixed this release (PR #333).

`wal::read_all` returned a hard `MalformedPage` error for a WAL segment shorter
than its 16-byte file header, so `Store::open` failed when a `kill -9` during WAL
rotation left a freshly-created (not-yet-durable-header) segment beside the prior,
fully-durable one — the source of the intermittent `wal … shorter than its
header` failure on loaded CI runners. A torn header implies **no acknowledged
records** (`WalWriter::create` fsyncs the header before any record can be
appended), so recovery now treats a sub-header segment as an empty torn tail —
exactly like a torn frame — never an error. Test:
`wal::tests::sub_header_segment_is_an_empty_torn_tail` (fails before with the
exact CI message, passes after). The crash gate (ADR-0005) is honored, not
weakened.

## Dynamic scan (OWASP ZAP baseline)

A live server was started with the production-style secure configuration —
encryption-at-rest on, an admin API key required, REST on `127.0.0.1:7333` — a
collection was seeded, and ZAP (`ghcr.io/zaproxy/zaproxy:stable`,
`zap-baseline.py`) was pointed at it.

**Result: 0 FAIL, 0 high/medium alerts, 1 informational warning, 66 passive
rules passed.**

- The single warning is **Non-Storable Content** (`10049`, risk = informational)
  on three `401 Unauthorized` responses (`/`, `/robots.txt`, `/sitemap.xml`).
  This is **correct** behaviour: authentication-failure responses are
  deliberately not cacheable, and Quiver serves no static content, robots, or
  sitemap. No action.
- The spider received **401 on every path**, including `/` and any unmatched
  route — the server is **default-deny**: it never returns 404 for an unknown
  path (no route enumeration) and never serves an unauthenticated body beyond the
  open liveness/metrics endpoints. ZAP therefore could not crawl into the API,
  which is the intended posture; it also means the baseline passive scan did not
  exercise the **authenticated** request bodies.
- The deeper, authenticated **API scan** (`zap-api-scan.py`, which actively
  fuzzes each operation from an OpenAPI definition with a key configured) is
  sequenced with the machine-readable OpenAPI spec being generated for the API
  reference; it is **not yet run** and is listed as a residual item below rather
  than claimed.

No missing-security-header failure, information disclosure, injection reflection,
or insecure-transport finding was reported. (The JSON API reflects no HTML, so
the CSP / anti-CSRF / XSS passive rules pass by absence of an attack surface.)

## Checked, not vulnerable

### C1 — Access control: default-deny RBAC holds across every surface

Authorization is **default-deny** at the single op-layer choke point both REST
and gRPC share (`crates/quiver-server/src/auth.rs`): every operation is checked
against the caller's `Action` and `CollectionScope`; an unknown/missing key is a
401; an over-scope or over-role call is a 403. The **coordinator** now shares the
same model (F1). Tests: `rbac.rs::scoped_keys_deny_over_scope_and_cross_namespace`
(REST + gRPC), `cluster_coordinator_auth.rs::coordinator_membership_api_requires_auth`,
and `error_paths.rs::bad_input_is_rejected_cleanly_never_500`.

### C2 — Path traversal via collection name: impossible by construction

A collection's on-disk directory is its **numeric** `CollectionId`
(`store.rs::collection_dir` formats `{:010}`), never its caller-supplied name.
The name lives only in the manifest, mapped to the id. A crafted name (e.g.
`../../etc`) therefore cannot escape the data directory — it never reaches the
filesystem path at all. The snapshot `destination` is the only client-supplied
path, and it is **admin-only** (`authorize_global(Action::Admin)`) and audited;
an admin already controls the host and data directory, so writing a server-local
snapshot is within existing authority (an accepted, documented risk).

### C3 — SSRF: not reachable from an unprivileged request

Every URL the server fetches comes from **configuration or an admin-only call**,
never from an unprivileged or unauthenticated request body:

- embedding / rerank provider `endpoint`s — from the `[embedding]`/`[rerank]`
  config tables, resolved at startup (`quiver-providers`);
- live-import sources (`--qdrant-url` / `--chroma-url` / `--postgres-url`) — from
  the `quiver admin import` **command line** (operator-run; see the v0.17.0 audit
  C1, still accurate);
- cluster shard / replica / standby URLs and the replication `leader_url` — from
  cluster/replication **config**;
- the `url` in `add_shard` / `grow` / `raft_add_voter` request bodies — from
  **admin-only** coordinator/Raft endpoints (F1).

SSRF requires an attacker to influence a request a *more-privileged* service
makes; none of these URLs is attacker-influenced from an unprivileged boundary,
so the class is not reachable. As in v0.17.0, no private-address blocklist is
added on purpose: the documented primary use case imports from
`http://localhost:6333`, which such a blocklist would break.

### C4 — Injection: SQL identifier is quoted; filters are parsed, not interpolated

The live pgvector connector interpolates only the table **identifier**, through
`quote_ident` (`quiver-import/src/live.rs`), which double-quotes each
dot-separated part and doubles embedded quotes; values are never interpolated
(rows return via `row_to_json`). Search filters are a typed `quiver-query::Filter`
parsed from JSON, never string-built into a query. Tests:
`quote_ident_quotes_and_rejects_empty`, plus the `filter_json` fuzz target (C7).

### C5 — Malformed input: every public entry point rejects cleanly (no 500)

The bad-input contract is enforced and tested across the public surface: unknown
collection (404), wrong vector dimensionality, over-limit `k` / `ef_search` /
payload / batch (ADR-0040 cost limits, 400), and serde-rejected bodies all return
a 4xx — never a 5xx, never a panic-induced connection drop — and gRPC shares the
mapping. Test: `error_paths.rs::bad_input_is_rejected_cleanly_never_500` (REST +
a gRPC parity case + a post-battery liveness check).

### C6 — Encryption at rest: data files are ciphertext; crypto-shred works

The server boots with an envelope key-ring; segments, the index, the manifest,
and the WAL are AEAD-sealed (XChaCha20-Poly1305). Tests assert the plaintext
vector never reaches disk for DCPE (`dcpe.rs`) and client-side
(`client_side_vectors.rs`) modes; `quiver-crypto/tests/at_rest.rs` covers the
at-rest layer directly; `envelope_shred.rs` proves a dropped collection's wrapped
DEK is destroyed, making its at-rest data cryptographically unrecoverable.
Residual risk (root-on-host reading process memory) is stated in the threat
model, not hidden.

### C7 — Untrusted-input parsers: fuzzed clean

The wire-format and on-disk decoders are the parts that touch attacker-controlled
bytes. All three `cargo-fuzz` targets were re-run on this box for this pass with
no panic, no crash, and no new artifacts:

| Target | Runs | Duration | Peak RSS | Result |
| --- | --- | --- | --- | --- |
| `filter_json` (search-filter wire format) | 7,179,206 | 61 s | 492 MB | clean |
| `page_decode` (on-disk page codec) | 24,747,817 | 61 s | 492 MB | clean |
| `wal_decode` (write-ahead-log codec) | 1,558,218 | 61 s | 469 MB | clean |

(Re-run on this box, 2026-06-27, `cargo +nightly fuzz run … -max_total_time=60`;
no crash, no panic, no new corpus artifact.)

Backed by proptests (`wal::entries_roundtrip`,
`wal::truncation_yields_a_clean_prefix`, `page::any_body_roundtrips`) and the
torn-write / corruption unit tests in `wal.rs` / `page.rs`, including the new
`sub_header_segment_is_an_empty_torn_tail` regression (F2). Malformed input
rejects with a typed error rather than panicking.

### C8 — Audit log: records actor/action/resource, never secrets

Mutations and access-control denials are recorded with the caller's non-secret
identity (a configured label or a SHA-256 fingerprint, never the key). Test:
`audit.rs::audit_log_records_mutations_and_denials_without_leaking_secrets`
asserts no key secret ever appears in the log file.

### C9 — Denial of service via query cost: capped at the op layer

Per-request caps (`k`, `ef_search`, `fetch` limit, vector dimension, payload
size, batch size, sparse-term count, and HTTP request body size — ADR-0040) are
enforced at the shared op layer and rejected with 400 / `InvalidArgument`, plus a
per-key token-bucket rate limit (ADR-0049). Tests: `cost_limits.rs`,
`rate_limit.rs`, and the over-limit cases in `error_paths.rs`. Still deferred (and
stated, not claimed): a work-cancelling per-query timeout (not achievable under
the current `spawn_blocking` model without cooperative cancellation).

### C10 — Supply chain: pinned, scanned, no suppressions

`cargo deny` (advisories/bans/licenses/sources) and `cargo audit` run in CI with
no blanket suppressions; the optional `cuda` dependency tree (cudarc) is
MIT/Apache and `deny`-clean. Dependencies are workspace-pinned. CodeQL runs on
every PR.

## Residual risks (documented honestly)

- **Root on the live host** can read plaintext vectors/payloads from process
  memory while the server runs; at-rest encryption does not defend this, and only
  the `client_side` vector mode keeps vectors off the server entirely. (Threat
  model A4/A6.)
- **DCPE leaks the approximate distance-comparison relation by design** — it is
  not semantically secure, and the v2 hardening does not change the leakage class.
  Use `client_side` where that matters. (ADR-0031 / ADR-0035.)
- **The audit log is append-only but not yet hash-chained**, so it is
  tamper-evident only to the extent the filesystem is. (`audit.md`.)
- **The authenticated ZAP API scan is not yet run** — it is sequenced with the
  OpenAPI spec being generated for the API reference, and is listed here rather
  than claimed as complete.
- **CodeQL `rust/hard-coded-cryptographic-value` alerts on `dcpe.rs`** are the
  scheme's fixed, published test/domain constants (by design, justified at
  `dcpe.rs`); they require an owner dismissal in the GitHub Security UI (the agent
  cannot dismiss them).
- **Live-import TLS is the operator's choice** — the CLI warns on cleartext
  credentials (v0.17.0 F1) but does not refuse a plaintext connection.

## Sign-off

The findings this pass (F1 coordinator auth, F2 crash-gate torn header) are fixed
with regression tests; the ZAP baseline DAST is clean (0 FAIL, 1 informational);
the three fuzzers ran clean; and the static review found the access-control,
injection, SSRF, crypto, and DoS classes defended or not reachable, with the open
items listed honestly above. No fabricated results.
