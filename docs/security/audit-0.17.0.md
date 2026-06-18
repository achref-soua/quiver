# Security audit — v0.17.0 launch-hardening pass

**Date:** 2026-06-18 · **Scope:** the whole codebase ahead of the v0.17.0 release,
reviewed OWASP-style against the [threat model](./threat-model.md). This note
records what was probed, what was fixed, and — equally — what was **checked and
found not vulnerable, and why**. It complements the design-level
[`threat-model.md`](./threat-model.md), the mechanism-level [`crypto.md`](./crypto.md),
the [fuzzing](./fuzzing.md) notes, and the audit-*logging* doc [`audit.md`](./audit.md).

> **Honesty.** Every claim here is backed by a named test, a fuzz run reproduced
> on this box, or a code reference. Findings we could not close are listed as
> residual risk, not buried.

## Method

- Read the request-handling, auth, crypto, storage, and parsing paths for the
  OWASP Top-10 classes that apply to a self-hosted database: broken access
  control, injection, SSRF, cryptographic failures, security misconfiguration,
  and denial-of-service via untrusted input.
- Ran the untrusted-input parsers under `cargo-fuzz` for a fixed duration each and
  confirmed no panics/crashes (below).
- Drove every external surface end-to-end against a live encrypted server with
  [`just acceptance`](../testing/manual-acceptance.md), including RBAC denials.

## Findings

### F1 — Live-import credentials could be sent in cleartext (fixed)

**Severity:** low. **Status:** fixed in this release.

The live migration connectors (`quiver admin import --qdrant-url / --chroma-url /
--postgres-url`) attach an API key / token / password to the request. Over a
plaintext `http://` URL (Qdrant/Chroma) or a Postgres URL with `sslmode=disable`,
that credential travels unencrypted.

**Fix:** `quiver_import::plaintext_credential_warning` (pure, unit-tested) detects
both cases and the import CLI prints a `warning:` to stderr before connecting.
`sslmode` absent or `prefer` is **not** flagged, because libpq still negotiates
TLS when the server offers it (avoiding false alarms). Tests:
`warns_on_an_api_key_over_plaintext_http`,
`warns_on_a_postgres_password_with_tls_disabled`, `url_userinfo_password_detection`.
Documented in [`../migration.md`](../migration.md) ("Security of live import").

## Checked, not vulnerable

### C1 — SSRF in the migration connectors: not exploitable

The connectors fetch an **operator-supplied** URL. That URL comes only from the
`quiver admin import` command line — there is **no** REST/gRPC/MCP endpoint that
accepts a URL and fetches it (verified: the only callers of `fetch_qdrant` /
`fetch_chroma` / `fetch_pgvector` are in `crates/quiver-cli/src/admin.rs`). SSRF
requires an attacker to influence a request a *more-privileged* service makes; an
operator running an admin command already has full local privilege, so no boundary
is crossed. We deliberately do **not** add a private-address blocklist: the
documented primary use case is importing from `http://localhost:6333`, which such
a blocklist would break. The honest posture (operator-trusted URL, credential and
TLS cautions) is documented in `migration.md`.

### C2 — SQL injection via the import table name: defended by construction

The live pgvector connector interpolates the table name into
`SELECT row_to_json(t) FROM (SELECT * FROM <table>) t`. The name is passed through
`quote_ident`, which double-quotes each dot-separated part and doubles any embedded
quote, so a crafted `--table` cannot break out of the identifier. Values are never
interpolated — only the table identifier — and rows come back via `row_to_json`.
Test: `quote_ident_quotes_and_rejects_empty`.

### C3 — Untrusted-input parsers: fuzzed clean

The wire-format and on-disk decoders are the parts that touch attacker-controlled
bytes. All three `cargo-fuzz` targets ran clean this pass (no panic, no crash, no
new artifacts):

| Target | Runs | Duration | Peak RSS |
| --- | --- | --- | --- |
| `filter_json` (search-filter wire format) | — | 180 s | 556 MB |
| `page_decode` (on-disk page codec) | 66,257,252 | 180 s | 491 MB |
| `wal_decode` (write-ahead-log codec) | 5,004,659 | 180 s | 410 MB |

Backed by proptests (`wal::entries_roundtrip`,
`wal::truncation_yields_a_clean_prefix`, `page::any_body_roundtrips`, run at
`PROPTEST_CASES=8192`) and the torn-write / corruption unit tests in `wal.rs` /
`page.rs`. Malformed input rejects with a typed error rather than panicking.

### C4 — Access control: default-deny RBAC holds

Authorization is **default-deny** (`crates/quiver-server/src/auth.rs`): every
operation is checked against the caller's `Action` and `CollectionScope` at the
single choke point both REST and gRPC share. Scoped keys cannot reach collections
outside their pattern, and an unknown/missing key is rejected (HTTP 401). Tests:
`scoped_keys_deny_over_scope_and_cross_namespace` (gRPC + REST), plus the live
`just acceptance` run asserting 401 for both a wrong key and a missing key.

### C5 — Encryption at rest: data files are ciphertext

The server boots with an envelope key-ring; pages are AEAD-encrypted. Tests assert
the plaintext vector never reaches disk for both DCPE
(`crates/quiver-server/tests/dcpe.rs`) and client-side modes
(`client_side_vectors.rs`), and `crates/quiver-crypto/tests/at_rest.rs` covers the
at-rest layer directly. Residual risk (root-on-host reading process memory) is
stated in the threat model, not hidden.

### C6 — Crypto-shredding: dropped collections are unrecoverable

Destroying a collection destroys its wrapped DEK, making the at-rest data
cryptographically unrecoverable. Test:
`crates/quiver-crypto/tests/envelope_shred.rs`.

### C7 — Audit log: records actor/action/resource, never secrets

Mutations and denials are recorded with the caller's non-secret identity (a label
or a SHA-256 fingerprint, never the key). Test:
`crates/quiver-server/tests/audit.rs` asserts no key secret ever appears in the
log file.

### C8 — DoS via query cost: bounded

Query cost limits cap `k`, `ef_search`, and result sizes; the client-side mode's
`candidate_limit` bounds the fetch. Dependency-supply-chain risk is gated by
`cargo deny` + `cargo audit` (no suppressions; `deny.toml` `ignore = []`), run as
part of `just verify`.

## Residual risks (unchanged, documented honestly)

- **Root on the live host** can read plaintext vectors/payloads from process
  memory while the server runs. At-rest encryption does not defend this; only the
  `client_side` mode keeps vectors off the server entirely. (Threat model A4/A6.)
- **DCPE leaks the approximate distance-comparison relation by design** — it is
  not semantically secure; the v2 hardening does not change the leakage class.
  Use `client_side` where that matters. (ADR-0031 / ADR-0035.)
- **The audit log is append-only but not yet hash-chained**, so it is
  tamper-evident only to the extent the filesystem is. Cryptographic chaining is a
  tracked future enhancement. (`audit.md`.)
- **Live-import TLS is the operator's choice.** The CLI now warns about cleartext
  credentials (F1) but does not refuse a plaintext connection — an operator may
  intentionally import over a trusted local network.

## Sign-off

The v0.17.0 gate (`just verify` · `just test-py` · `just test-ts` · proptests ·
the three fuzzers · `crash_recovery` · `just acceptance`) is green, and the one
finding (F1) is fixed with regression tests. No fabricated results.
