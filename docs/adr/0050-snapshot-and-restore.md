# ADR-0050: Online snapshot & restore

- **Status:** Accepted
- **Date:** 2026-06-22
- **Deciders:** Achref Soua

## Context

The Quiver data directory is already portable — stop the process, copy the
directory, start it elsewhere, and it opens. But there is no *online* way to
capture a consistent copy of a running store, and no documented restore path.
Operators expect a backup primitive: "snapshot this live database to a target
directory" and "restore a snapshot into a fresh data directory."

The store is an LSM-style, single-writer engine (ADR-0005, ADR-0006). Mutations
go through a write-ahead log; a `checkpoint` seals the active buffer into
immutable segments and atomically swaps the manifest (ADR-0004), advancing
`last_checkpointed_lsn` to the WAL head. Immutable segments and the
atomic-rename manifest protocol are what make a consistent copy *possible*; the
crash gate (ADR-0005) must remain untouched — a snapshot is an additive read of
the source, never a mutation of its durability path.

## Decision

Add an engine-level snapshot/restore, exposed up the stack:

- **`Database::snapshot(dest)`** — under the single-writer lock (so no mutation
  is in flight): `checkpoint()` first, which flushes the active buffer into
  segments and advances `last_checkpointed_lsn` to the WAL head, then perform a
  byte copy of the entire data directory into `dest`. Because the checkpoint
  makes the manifest the durability anchor and the writer lock is held, the
  copied tree is internally consistent at one LSN: opening it replays an empty
  WAL tail and yields a database identical to the source at snapshot time.
  Returns a `SnapshotInfo { manifest_version, files, bytes }`.

- **`restore_snapshot(src, dest)`** — a free function that copies a snapshot
  directory into a fresh `dest` (which must not already exist), then verifies it
  opens. The caller then opens `dest` normally (with the same keyring/codec the
  snapshot was written under). Restore is deliberately *offline with respect to
  a running server*: it produces a data directory; swapping a live server onto
  it is an operator action (point the server at the restored directory), not a
  hot in-process swap, which would be a far riskier surface for little gain.

- **Surfaces:** REST `POST /v1/snapshot { destination }` (server-local path);
  MCP `snapshot` tool; and `database_stats` reports the snapshot-relevant
  catalog state (`manifest_version`, on-disk `bytes`) so an agent can see what a
  snapshot would capture.

Consistency comes from `checkpoint` + the writer lock; the copy itself is a
whole-directory byte copy for **layout independence** — it does not need to know
the `.vec`/`.pay`/`.dir`/`.del`/`.sec`/`index/` file grammar, so it cannot drift
out of sync with the storage format. This is a derived artifact: no on-disk
format changes, the manifest schema is untouched, and the crash gate is
unaffected.

## Consequences

- **+** A real online backup primitive with one consistent LSN, reusing the
  existing checkpoint + immutable-segment machinery; zero on-disk format change.
- **+** Layout-independent: the copier never parses segment files, so new
  storage artifacts are captured automatically.
- **−** A snapshot is a full copy, O(data size) in time and space. For large
  stores an incremental / hard-linked snapshot (immutable segments never change,
  so they can be hard-linked; only the manifest + new segments differ) is the
  obvious optimization — noted as the upgrade path, not built now (YAGNI until a
  measurement shows copy time hurts).
- **−** The snapshot briefly holds the writer lock for the checkpoint; the bulk
  copy also holds it (single-writer engine), so writes pause for the duration.
  Acceptable for a backup operation; the hard-link optimization shortens it.
- **−** S3 / remote export is **not** built here — the snapshot writes to a local
  path, and `aws s3 cp`/`rclone` over that path is the documented pattern. A
  native object-store exporter is a later, optional addition (no new dependency
  added speculatively).

## Alternatives considered

- **Selective, manifest-driven file copy** (copy only the files the current
  manifest references) — more code, couples the copier to the storage-file
  grammar, and saves nothing over the whole-dir copy once `checkpoint` has run
  (the WAL tail is empty). Rejected for the layout-independent whole-dir copy.
- **In-process hot restore** (swap a running server onto a restored directory) —
  rejected: replacing a live `Database` under concurrent readers is a sharp,
  low-value surface; restoring to a directory and pointing the server at it is
  simpler and safer.
- **Filesystem/LVM snapshots** — out of scope and not portable across deploy
  targets; the engine-level copy works anywhere.

## Implementation

`Database::snapshot(dest)` (quiver-embed) holds the writer lock, `checkpoint()`s, then byte-copies the data directory; `restore_snapshot(src, dest)` copies a snapshot into a fresh directory. Exposed as REST `POST /v1/snapshot` (admin), the MCP `snapshot` tool + `database_stats` status, and `snapshot()` in the Go / Python / TypeScript SDKs.

## Verification

embed unit tests (snapshot→open reproduces the DB incl. a post-snapshot write that must not appear; refuses an existing dest; restore roundtrip + guards), a REST e2e (snapshot opens as an identical DB; second snapshot → 409), an MCP test, and SDK tests.
