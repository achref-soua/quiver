# Audit logging

Quiver records security-relevant events to an **append-only audit log** so that
who did what, to which resource, and with what outcome is reconstructable after
the fact ([ADR-0011](../adr/0011-authn-authz-tenancy.md); the threat model's
*repudiation* control). It is part of the observability surface
([ADR-0014](../adr/0014-observability.md)) but is distinct from ordinary request
logs.

## What is recorded

At the single authorization choke point that both REST and gRPC share
(`AppState`), Quiver records:

- **Every mutating operation** — `create_collection`, `delete_collection`,
  `upsert`, `delete_points` — with its outcome (`ok` or `error`).
- **Every access-control denial** — any operation an API key's role or
  collection scope forbids — with outcome `denied`.

Successful **reads** (`get_collection`, `get_points`, `search`,
`list_collections`) are deliberately **not** recorded: they do not change state
and would swamp the signal. A *denied* read still is — a denial is a security
event regardless of the action.

## Record format

Each record is one JSON object on its own line (JSON Lines / `ndjson`):

```json
{"ts_ms":1718370000123,"actor":"ci-admin","action":"upsert","resource":"acme.docs","outcome":"ok"}
{"ts_ms":1718370000456,"actor":"key:9f86d081884c7d65","action":"upsert","resource":"acme.docs","outcome":"denied"}
```

| Field | Meaning |
| --- | --- |
| `ts_ms` | Unix epoch milliseconds, UTC — a timezone-free, sortable integer (no date dependency). |
| `actor` | The caller's non-secret identity — see below. |
| `action` | The operation: `create_collection`, `delete_collection`, `upsert`, `delete_points`, or, for a denial, the operation the caller attempted (including reads such as `get_collection` or `search`). |
| `resource` | The target collection, or `*` for a collection-agnostic operation (listing). |
| `outcome` | `ok`, `denied`, or `error`. |

## Actor identity — never the secret

The audit log must attribute an action to a key without ever revealing the key.
Each `ApiKey` therefore has an optional non-secret `id`:

- if set, the `actor` is that label verbatim (e.g. `ci-admin`);
- if unset, the `actor` is `key:<fingerprint>`, where the fingerprint is the
  first eight bytes of the SHA-256 of the secret, hex-encoded. SHA-256 is
  preimage-resistant, so the fingerprint identifies a key consistently yet
  cannot be reversed into the secret.

In `insecure` mode (no keys configured) the actor is `insecure`.

`quiver-server/tests/audit.rs` asserts end-to-end that no key secret ever appears
in the log file.

## Configuration

Set `QUIVER_AUDIT_LOG` (or `audit_log` in `quiver.toml`) to a file path:

```bash
QUIVER_AUDIT_LOG=/var/log/quiver/audit.log
```

When unset, records are still emitted as structured `tracing` events under the
target `quiver::audit` — so they appear in the server logs and can be shipped by
any tracing/OpenTelemetry exporter; only the JSON-Lines **file** is skipped.

## Operational notes

- **Append-only:** the file is opened with `O_APPEND` and never truncated. Writes
  are serialized behind a mutex and flushed per line, so records never
  interleave.
- **Rotation** is operator-managed (e.g. `logrotate` with `copytruncate`, or
  point `QUIVER_AUDIT_LOG` at a fresh path and restart). The server holds the
  file open, so a plain rename keeps writing to the old inode until restart;
  SIGHUP-reopen is a future enhancement.
- **Availability over fail-closed:** if an audit write fails (for example, a full
  disk), the failure is logged loudly via `tracing` but the caller's operation
  still proceeds. A strict fail-closed mode (refuse any operation that cannot be
  audited) is a documented future option.
- **Tamper-evidence:** the log is append-only but not yet hash-chained;
  cryptographic chaining for tamper-evidence (ADR-0011) is a future enhancement.
