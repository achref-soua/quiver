# Read-during-write contention — does MVCC pay for itself?

This is the **measure-first** gate for lock-free MVCC reads ([ADR-0064](../../adr/0064-mvcc-reads-implementation.md)).
Both ADR-0053 and ADR-0064 say to justify the L–XL reclamation work with a measured
read-concurrency ceiling before building it. The sweep
(`quiver_bench.contention_sweep`) measures concurrent read QPS **with** vs
**without** concurrent writers; the ratio is the penalty the current `RwLock`
([ADR-0057](../../adr/0057-concurrent-reads-rwlock.md)) imposes — a write takes the
exclusive lock (one WAL fsync) and blocks every read for its duration — which MVCC
would remove.

A single writer is only the **floor** of the write pressure. ADR-0064 says the
penalty grows with write **concurrency** (more exclusive-lock acquisitions
competing with readers) and write **size** (a longer lock window per fsync), so the
sweep now measures a **grid of both** — writer-thread counts × upsert batch sizes —
against one read-only baseline.

## Measured (SIFTSMALL, dev box · indicative)

8 reader threads, `ef_search=64`, 4 s per phase. Read-only baseline = **1166 QPS**.
Each cell is the read QPS **retained** under that write pressure (1.0× = no penalty):

| readers vs.        | batch 1 | batch 64 | batch 512 |
| ------------------ | ------- | -------- | --------- |
| **1 writer**       | 0.83×   | 0.75×    | 0.55×     |
| **2 writers**      | 0.10×   | 0.47×    | 0.33×     |
| **4 writers**      | 0.00×   | 0.03×    | 0.17×     |

> WSL2 shared dev box, single Quiver process, in-memory HNSW. The Python client is
> itself a concurrency ceiling, so **absolute QPS is indicative, not a headline**
> (reference-hardware-pending). The **ratio**, and its *shape*, is the honest signal —
> and a second run reproduced it (baseline 1268 QPS; the `2 writers · batch 1` and
> `4 writers · batch 1` cells collapsed to 0.02× and 0.00× again). CSV:
> [`contention/contention_sweep.csv`](contention/contention_sweep.csv).

## Reading it

- **One writer is moderate (≈0.8× small / 0.55× large) — and it is the best case.**
  The earlier single-writer figure (0.73×) sits in this row; on its own it reads as
  "tolerable."
- **Write *concurrency* is the dominant killer, and it is catastrophic, not moderate.**
  A *second* concurrent writer of small upserts already collapses read throughput to
  **0.10×**; four writers **starve readers to ~0** (5–6 QPS). Under a `RwLock`, every
  writer that wants the exclusive lock blocks all readers, and a steady stream of
  write acquisitions leaves almost no window for reads.
- **Counter-intuitively, bigger batches retain *more* read QPS at high writer counts**
  (4 writers: 0.00× at batch 1 vs 0.17× at batch 512). It is the **rate of
  exclusive-lock acquisitions** that starves readers, not the bytes written — many
  tiny writes grab and release the lock far more often than a few large ones, so they
  leave fewer gaps for readers. Write *size* matters at one writer (0.83×→0.55×);
  write *concurrency* dominates everywhere else.
- **Implication for MVCC:** the gate is **met, decisively**. The single-writer 0.73×
  understated the problem; any read-heavy workload with more than one concurrent
  writer leaves the overwhelming majority of read throughput on the table under the
  `RwLock`. MVCC ([ADR-0064](../../adr/0064-mvcc-reads-implementation.md)) lets reads
  proceed *during* writes, targeting the gap back toward 1.0× — this is the measured
  evidence the L–XL build was gated on, not an adjective.

## Before / after MVCC (the proof)

After the lock-free MVCC read path landed (ADR-0064, increments 1–3), the **same
sweep on the same box** with `QUIVER_MVCC_READS` off vs on. Off is the `RwLock`
path above; on serves pure-vector reads from the lock-free snapshot. **Retained
read-QPS ratio** (1.0× = no penalty):

| readers vs.        | batch 1 (off→on) | batch 64 (off→on) | batch 512 (off→on) |
| ------------------ | ---------------- | ----------------- | ------------------ |
| **1 writer**       | 0.80× → 0.87×    | 0.66× → 0.71×     | 0.50× → 0.74×      |
| **2 writers**      | **0.00× → 0.79×**| 0.35× → 0.75×     | 0.30× → 0.75×      |
| **4 writers**      | **0.01× → 0.67×**| 0.06× → 0.69×     | 0.07× → 0.71×      |

> SIFTSMALL, dev box (WSL2), 8 readers, `ef_search=64`, 4 s/phase. Reproduced
> across two runs (the multi-writer collapse→recovery is the robust headline). CSV:
> [`contention/contention_sweep.csv`](contention/contention_sweep.csv).

- **The collapse is gone.** Where the `RwLock` starved readers to near zero under
  concurrent writers (0.00×–0.07×), MVCC holds **~0.65×–0.79×** — reads proceed
  *during* writes instead of blocking on the writer's exclusive lock. The single-
  writer cases improve too (0.50×→0.74× at the largest batch).
- **The ratio is the honest signal; absolute QPS is `reference-hardware-pending`.**
  The dev box is shared WSL2 and the single-process Python client (GIL + HTTP) is
  itself a concurrency ceiling, so the headline read-QPS numbers are indicative, not
  authoritative. A side effect of that ceiling: under MVCC the unblocked readers
  consume more of the client's GIL, so the *writer* threads issue fewer writes/s —
  a client artifact, not an engine limit. The before/after **ratio on identical
  hardware** is the real proof the lock-free path earned its complexity.
- **Default-off.** MVCC ships opt-in (`QUIVER_MVCC_READS`); the proven `RwLock`
  path stays the default until this is validated on dedicated hardware.

## Reproduce

```bash
# prebuild the server binary first; never `cargo build` mid-run on the shared box
cargo build --release -p quiverdb-cli
PATH="$PWD/target/release:$PATH" uv run --project bench \
  python -m quiver_bench.contention_sweep \
    --dataset siftsmall --workers 8 --duration 4 \
    --writers 1,2,4 --batches 1,64,512 --mvcc both \
    --out docs/benchmarks/results/contention
```
