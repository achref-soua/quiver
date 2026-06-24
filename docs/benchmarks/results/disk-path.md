# Disk-resident path — recall@10 and memory footprint

The memory-frugality headline (ADR-0007, ADR-0019): the disk-resident DiskANN
index keeps only **product-quantized codes in RAM** while the graph and
full-precision vectors live in the **encrypted on-disk index**, with an exact
re-rank. This page records what is measured and host-independent, and is explicit
about what requires reference hardware. We never fabricate results.

> **`v0.23.0`:** the disk index is now **durable** (ADR-0063) — a server
> *loads* the `mmap`'d base on open instead of rebuilding it from every
> full-precision vector. Before this, a restarted **server** paid an `O(N)`
> full-RAM rebuild on open and served from that rebuild's allocator high-water
> mark, so its post-restart RSS did not reflect the frugal footprint (the
> benchmark harness measured exactly that). The footprints below were always
> correct for the *index itself* (the `disk_recall` example opens the artifact
> directly); the durable load makes the **server** serve from the same frugal
> path, so the head-to-head serving RSS below is now representative of a real
> deployment — still reference-hardware-pending for the absolute headline.

## Measured — SIFTSMALL (10,000 × 128-d, 100 queries, L2)

A real run of `examples/disk_recall` (build the Vamana graph + PQ codebook, write
the encrypted disk index, query it through `mmap`):

| `l_search` | recall@10 | QPS (1T, indicative) |
|---|---|---|
| 16  | 0.8720 | 10996 |
| 32  | 0.9680 |  6893 |
| 64  | 0.9980 |  3447 |
| 128 | 1.0000 |  1609 |

- **PQ subspaces** `m = 16` (8 dims/subspace); build 7.3 s.
- **RAM-resident codes: 0.2 MB** vs full-precision vectors **5.1 MB** — **32× smaller**.
- Encrypted on-disk index: 7.0 MB.

Recall and the byte footprints are properties of the index and the data, so they
are **host-independent and stand**. QPS is single-thread, against the index
directly on a resource-shared WSL2 box — **indicative only**.

## The headline: RAM-resident footprint at scale (exact)

Only the PQ codes (`m` bytes/vector) are resident; full vectors (`dim × 4` bytes)
stay on SSD. This is exact arithmetic, independent of the host:

| Dataset | Full-precision RAM | Disk-path RAM (PQ codes) | Reduction |
|---|---|---|---|
| SIFT 128-d, 1 M | 512 MB | 16 MB (`m=16`) | 32× |
| SIFT 128-d, 10 M | 5.12 GB | 160 MB (`m=16`) | 32× |
| 768-d embeddings, 10 M | 30.7 GB | 960 MB (`m=96`) | 32× |
| 768-d embeddings, 100 M | 307 GB | 9.6 GB (`m=96`) | 32× |

So a 10 M × 768-d collection that needs **~31 GB of RAM** as full-precision vectors
serves from **~1 GB of RAM** plus the OS-resident working set on the disk path —
the wedge. The on-disk index grows roughly linearly (≈ `dim×4 + R×4` bytes/vector
plus page overhead; ~7 GB at SIFT-128 / 10 M).

## Reference-hardware-pending

Per [`../methodology.md`](../methodology.md), the head-to-head **vs Qdrant and
LanceDB** — steady-state process **RSS**, saturated **QPS**, and p50/p95/p99
latency at a fixed recall operating point — requires identical dedicated hardware
configured per each system's own recommendations. The shared dev box is not a
source for those figures, and they are not invented here. The reporting template
and competitor-configuration rules are in the methodology.

## Reproduce

The example builds and serves in two phases, so the frugal **serve-time** RSS can
be measured on the `serve` process alone:

```bash
# build the encrypted disk index (RAM-heavy, one-time)
cargo run --release --example disk_recall -- build \
  bench/datasets/siftsmall/siftsmall_base.fvecs /tmp/sift.qvx
# serve it (only PQ codes resident) — measure this process's RSS
cargo run --release --example disk_recall -- serve \
  /tmp/sift.qvx bench/datasets/siftsmall/siftsmall_query.fvecs \
  bench/datasets/siftsmall/siftsmall_groundtruth.ivecs
```

The same example runs at 1 M / 10 M scale (the build is slower). To produce the
full head-to-head **vs Qdrant and LanceDB on dedicated hardware**, follow the
[reference-hardware runbook](../reference-hardware-runbook.md).

> Measured on: WSL2 (Linux 6.6, x86-64), release profile, single thread, shared
> box — recall and footprints are host-independent; throughput and the competitor
> RSS comparison are reference-hardware-pending.
