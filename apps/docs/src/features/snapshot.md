# Snapshots & backup

Quiver can take a **consistent online snapshot** of a running database — a backup
captured at one point in its history, without stopping the process (ADR-0050).

## How it works

Under the single-writer lock (so no mutation is in flight), the engine:

1. **Checkpoints** — seals the in-memory write buffer into immutable segments and
   advances the write-ahead-log floor to the head. This makes the manifest the
   durability anchor for the snapshot.
2. **Copies** the entire data directory to the destination. The copy is
   *layout-independent* — it never parses the `.vec`/`.pay`/`.dir`/`index/` file
   grammar — so it captures new storage artifacts automatically and can never
   drift out of sync with the on-disk format.

Opening the copy replays an empty WAL tail and yields a database **identical to
the source at snapshot time**. No on-disk format changes; the crash gate is
untouched.

## Taking a snapshot

REST (admin-scoped):

```bash
curl -X POST http://localhost:8080/v1/snapshot \
  -H "Authorization: Bearer $QUIVER_API_KEY" \
  -d '{"destination": "/backups/quiver-2026-06-23"}'
# → {"manifest_version": 12, "files": 48, "bytes": 10485760}
```

It is also available on the embeddable engine (`Database::snapshot`), the MCP
`snapshot` tool, and every SDK (`snapshot(destination)` in Python, TypeScript,
and Go). The `database_stats` MCP tool reports the snapshot-relevant catalog
state (`manifest_version`, `disk_bytes`).

The destination must not already exist (Quiver never overwrites a directory).

## Restoring

Restore is an operator action: copy a snapshot into a fresh data directory and
point a Quiver instance at it (`restore_snapshot(src, dest)` does the copy and
guards the engine). Because the snapshot is just a portable data directory, you
can also archive it to S3/object storage with `aws s3 cp` / `rclone`.

## Notes & limits

- A snapshot is a **full copy** (O(data size)). For very large stores, a
  hard-linked incremental snapshot is the documented optimization (immutable
  segments never change), not built yet.
- The writer is paused for the duration (single-writer engine) — fine for a
  backup operation.
- Run snapshots on a writable node; a read-only replica is backed up at the
  filesystem level instead.
