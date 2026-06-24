# Read-during-write contention — does MVCC pay for itself?

This is the **measure-first** gate for lock-free MVCC reads ([ADR-0064](../../adr/0064-mvcc-reads-implementation.md)).
Both ADR-0053 and ADR-0064 say to justify the L–XL reclamation work with a measured
read-concurrency ceiling before building it. The sweep
(`quiver_bench.contention_sweep`) measures concurrent read QPS **with** vs
**without** a concurrent writer; the ratio is the penalty the current `RwLock`
([ADR-0057](../../adr/0057-concurrent-reads-rwlock.md)) imposes — a write takes the
exclusive lock (one WAL fsync) and blocks every read for its duration — which MVCC
would remove.

## Measured (SIFTSMALL, dev box · indicative)

```
read-only        : 997 QPS   (8 reader threads)
read+1 writer    : 732 QPS   (8 readers + 1 continuous upserter)
retained         : 0.73x
```

> WSL2 shared dev box, single Quiver process, in-memory HNSW, `ef_search=64`, 4 s
> per phase, the Python client itself a concurrency ceiling. **Absolute QPS is
> indicative, not a headline** (reference-hardware-pending). The **ratio** is the
> honest signal and it is real: a *single* concurrent writer already costs ~27% of
> read throughput, because each upsert's exclusive lock serializes the readers.

## Reading it

- The penalty is **real but moderate at one writer**: 0.73×. It grows with write
  concurrency and write size (more/larger exclusive-lock windows), and shrinks
  toward 1.0 for read-only or write-rare workloads.
- **Implication for MVCC:** there is a measured case — a write-heavy, read-heavy
  mixed workload leaves throughput on the table under the `RwLock`. MVCC
  (ADR-0064) lets reads proceed *during* writes, targeting the gap back toward
  1.0×. The decision to build increments 1–2 should weigh this against the L–XL
  reclamation cost; this number is the evidence, not an adjective.

## Reproduce

```bash
PATH="$PWD/target/release:$PATH" uv run --project bench \
  python -m quiver_bench.contention_sweep \
    --dataset sift1m --workers 8 --duration 10 --ef 64 \
    --out docs/benchmarks/results/contention
```

Run it again after MVCC increment 3 lands to record the retained-ratio delta on
the **same** box — that before/after, on identical hardware, is the real proof the
change earned its complexity (we never fabricate the numbers).
