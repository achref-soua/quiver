# ADR-0005: Durability & crash recovery

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

A database's first duty is to not lose acknowledged data and to never serve corrupted data. Quiver must survive process kill and power loss with a precise, testable guarantee.

## Decision

**Durability guarantee:** an upsert/delete is acknowledged to the client *only after* its record is durably appended to the **write-ahead log** (`fsync`'d). That write then survives crash and restart.

**Mechanism:**

- **WAL** — append-only, rotated log of length-prefixed, CRC32C-framed records (`Upsert`, `Delete`, `CreateCollection`, `DropCollection`, `Checkpoint`), each tagged with a monotonic **LSN**. The active in-memory segment and the live index are only updated after the WAL append.
- **fsync policy** — default **per-commit fsync** for strict durability; an optional **group-commit** window batches concurrent commits into one fsync to raise throughput without weakening the guarantee (a commit returns only once its batch is fsync'd). The directory is fsync'd after file creation/rename.
- **Checkpointing** — sealing/flushing the active segment to immutable files and publishing a new **manifest** version advances the `last_checkpointed_LSN`; WAL segments fully below it become reclaimable.
- **Atomic catalog** — manifest updated via write-new + fsync + atomic-rename of `CURRENT` (ADR-0004), so the live set flips atomically.

**Recovery algorithm (on open):**

1. Read `CURRENT` → load the manifest → establish `last_checkpointed_LSN` and the live segment set.
2. Replay every WAL record with `LSN > last_checkpointed_LSN` **idempotently** into a fresh active segment and the index (idempotent because records carry LSNs and target deterministic rows).
3. **Discard a torn trailing WAL record** — its framing CRC fails, proving it was a partial, never-acknowledged append.
4. **Garbage-collect orphans** — segment/index files not referenced by the manifest (a crash between file flush and manifest swap) are deleted.

## Consequences

- **+** Acknowledged writes survive `kill -9`/power loss; partial writes are detected and dropped; recovery is deterministic and idempotent.
- **−** Per-commit fsync caps single-writer commit latency at device fsync latency; group commit mitigates this for concurrent load. An optional relaxed mode (fsync-on-interval) may be offered later with an explicit, documented weaker guarantee — **off by default**.

## Verification

A crash-recovery harness forks a writer and `SIGKILL`s it at randomized points (mid-page-write, mid-WAL-append, between flush and manifest swap), reopens the store, and asserts: every acknowledged write present; no torn page accepted (CRC/AEAD catches it); WAL replay idempotent. This **gates `v0.1.0`** (risk R3).

## Alternatives considered

- **No WAL, fsync segments directly** — rejected: large write amplification, no fine-grained durability, painful partial-write recovery.
- **mmap + msync as the durability primitive** — rejected: imprecise flush ordering and fsync semantics across platforms; the WAL gives explicit control.
- **Relaxed durability by default** — rejected: a database must be safe by default; relaxation is opt-in and documented.
