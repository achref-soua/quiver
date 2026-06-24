# SPDX-License-Identifier: AGPL-3.0-only
"""Read-during-write contention sweep — Quiver only (ADR-0064, measure-first).

Measures concurrent read QPS **with** vs **without** a concurrent writer. Today
the server holds an `RwLock` (ADR-0057), so a write takes the exclusive lock and
blocks every read for its duration (an upsert is one WAL fsync). The ratio

    read_qps(under concurrent writes) / read_qps(read-only)

is the contention penalty lock-free MVCC reads (ADR-0064) would remove. This sweep
exists to *justify* (or not) that L-XL change before it is built: if the ratio is
near 1.0 the RwLock is not a real ceiling on this workload and MVCC is not worth
the reclamation complexity; if it collapses, MVCC has a measured case.

A single writer is only the floor of the write pressure. The penalty grows with
write *concurrency* (more exclusive-lock acquisitions) and write *size* (a longer
lock window per fsync), so the sweep measures a grid of both — writer-thread counts
× upsert batch sizes — against one read-only baseline. The resulting
retained-ratio-vs-write-pressure table is the measured ceiling.

Run against a freshly built server (the adapter starts one)::

    uv run --project bench python -m quiver_bench.contention_sweep \
        --dataset siftsmall --workers 8 --duration 4 \
        --writers 1,2,4 --batches 1,64,512 --mvcc both \
        --out docs/benchmarks/results/contention

`--mvcc both` runs the grid with `QUIVER_MVCC_READS` off and on (a fresh server
each) — the before/after that shows whether lock-free reads (ADR-0064) remove the
penalty. Pure-vector reads (the sweep's `query_one`) take the lock-free fast path.

QPS on a shared dev box is indicative, not authoritative — the reference-hardware
figure stays pending (we never fabricate it). The *ratio* is the honest signal.
"""

from __future__ import annotations

import argparse
import logging
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path

from .comparison import _load_dataset, _write_csv
from .competitors.base import BenchResult
from .competitors.quiver_adapter import QuiverAdapter

log = logging.getLogger("contention")
K = 10


def _drive(
    read_fn: Callable[[], None],
    workers: int,
    duration_s: float,
    write_fn: Callable[[], None] | None = None,
    n_writers: int = 1,
) -> tuple[int, int, float]:
    """Run `workers` reader threads (and `n_writers` writer threads if `write_fn`)
    in a busy loop for `duration_s`, returning `(total_reads, total_writes,
    elapsed_s)`.

    Pure given its callables, so it is unit-tested with fakes — no server. The
    reads/writes are HTTP calls that release the GIL, so the server-side
    read/write concurrency is what is actually exercised. `n_writers > 1` is the
    write-*concurrency* pressure dimension (more exclusive-lock acquisitions
    competing with the readers)."""
    stop = threading.Event()
    reads = [0] * workers
    n_w = n_writers if write_fn is not None else 0
    writes = [0] * max(n_w, 1)

    def reader(slot: int) -> None:
        n = 0
        while not stop.is_set():
            read_fn()
            n += 1
        reads[slot] = n

    def writer(slot: int) -> None:
        n = 0
        while not stop.is_set():
            write_fn()  # type: ignore[misc]
            n += 1
        writes[slot] = n

    threads = [threading.Thread(target=reader, args=(i,)) for i in range(workers)]
    threads += [threading.Thread(target=writer, args=(i,)) for i in range(n_w)]

    t0 = time.perf_counter()
    for t in threads:
        t.start()
    # Sleep the wall-clock window, then signal stop and drain.
    time.sleep(duration_s)
    stop.set()
    for t in threads:
        t.join()
    return sum(reads), sum(writes) if n_w else 0, time.perf_counter() - t0


def run_contention(
    adapter: QuiverAdapter,
    base,
    queries,
    dataset_name: str,
    *,
    workers: int,
    duration_s: float,
    ef_search: int,
    writer_counts: list[int],
    batch_sizes: list[int],
    mvcc: bool = False,
) -> list[BenchResult]:
    """Build, then measure read QPS read-only and across a grid of write pressure:
    `writer_counts` (write *concurrency*) × `batch_sizes` (write *size* per upsert).

    The read-only baseline is measured once; every grid cell's retained ratio is
    relative to it. The two dimensions are exactly the knobs ADR-0064 says the
    RwLock penalty grows with — so the resulting table is the measured ceiling that
    justifies the L-XL MVCC build or retires it."""
    import itertools

    import numpy as np

    adapter.start()
    try:
        build_s, _ = adapter.build(base, metric="l2")
        log.info("built in %.1fs; measuring contention (workers=%d)", build_s, workers)

        rng = np.random.default_rng(42)
        n = base.shape[0]

        def read_fn() -> None:
            q = queries[rng.integers(0, queries.shape[0])]
            adapter.query_one(q, K, ef_search)

        def make_write_fn(batch: int) -> Callable[[], None]:
            # A GIL-atomic counter walks contiguous id windows — no shared rng in
            # the writer hot path (numpy Generator isn't thread-safe across the
            # several writer threads), and re-upserting existing ids exercises the
            # live incremental-update path.
            cursor = itertools.count()

            def write_fn() -> None:
                start = (next(cursor) * batch) % n
                idx = [(start + j) % n for j in range(batch)]
                adapter.write_batch([str(i) for i in idx], [base[i] for i in idx])

            return write_fn

        def result(notes: str, qps: float, cfg: dict) -> BenchResult:
            return BenchResult(
                competitor="quiver",
                dataset=dataset_name,
                param_name="ef_search",
                param_value=ef_search,
                qps_nt=qps,
                concurrency=workers,
                build_s=build_s,
                notes=notes,
                config=cfg,
            )

        # Baseline: readers only, measured once. Every cell's ratio is vs this.
        r0, _, e0 = _drive(read_fn, workers, duration_s)
        read_only_qps = r0 / e0 if e0 > 0 else 0.0
        log.info(
            "[mvcc=%s] read-only baseline: %.0f QPS (%d readers)",
            mvcc,
            read_only_qps,
            workers,
        )
        results = [
            result("read_only", read_only_qps, {"writers": 0, "batch": 0, "ratio": 1.0, "mvcc": mvcc})
        ]

        # Grid: read QPS retained under each (writers × batch) write pressure.
        for nw, batch in itertools.product(writer_counts, batch_sizes):
            r1, w1, e1 = _drive(read_fn, workers, duration_s, make_write_fn(batch), n_writers=nw)
            qps = r1 / e1 if e1 > 0 else 0.0
            ratio = qps / read_only_qps if read_only_qps > 0 else 0.0
            writes_per_s = w1 / e1 if e1 > 0 else 0.0
            log.info(
                "[mvcc=%s] writers=%d batch=%-4d | %.0f QPS | retained %.2fx | %.0f writes/s (%.0f pts/s)",
                mvcc,
                nw,
                batch,
                qps,
                ratio,
                writes_per_s,
                writes_per_s * batch,
            )
            results.append(
                result(
                    f"read_during_write w{nw} b{batch} mvcc={mvcc}",
                    qps,
                    {
                        "writers": nw,
                        "batch": batch,
                        "ratio": round(ratio, 3),
                        "writes_per_s": round(writes_per_s, 1),
                        "mvcc": mvcc,
                    },
                )
            )
        return results
    finally:
        adapter.stop()


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    logging.getLogger("httpx").setLevel(logging.WARNING)  # one line per request is noise
    p = argparse.ArgumentParser(description="Quiver read-during-write contention sweep")
    p.add_argument("--dataset", default="siftsmall")
    p.add_argument("--datasets-dir", type=Path, default=Path("bench/datasets"))
    p.add_argument("--out", type=Path, default=Path("bench/results/contention"))
    p.add_argument("--quiver-url", default="http://127.0.0.1:6333")
    p.add_argument("--quiver-key", default=None)
    p.add_argument("--workers", type=int, default=8, help="concurrent reader threads")
    p.add_argument("--duration", type=float, default=4.0, help="seconds per phase")
    p.add_argument("--ef", type=int, default=64)
    p.add_argument(
        "--writers",
        default="1,2,4",
        help="comma-separated writer-thread counts (write-concurrency sweep)",
    )
    p.add_argument(
        "--batches",
        default="1,64,512",
        help="comma-separated upsert batch sizes (write-size sweep)",
    )
    p.add_argument(
        "--mvcc",
        default="both",
        choices=["off", "on", "both"],
        help="run with QUIVER_MVCC_READS off, on, or both (the before/after, ADR-0064)",
    )
    args = p.parse_args(argv)

    writer_counts = [int(x) for x in args.writers.split(",") if x]
    batch_sizes = [int(x) for x in args.batches.split(",") if x]
    modes = {"off": [False], "on": [True], "both": [False, True]}[args.mvcc]

    ds = _load_dataset(args.dataset, args.datasets_dir)
    results: list[BenchResult] = []
    for mvcc in modes:
        # A fresh server per mode so the MVCC flag is applied at open and the data
        # dir is clean (the adapter starts its own process).
        adapter = QuiverAdapter(
            url=args.quiver_url, api_key=args.quiver_key, start_server=True, mvcc=mvcc
        )
        results += run_contention(
            adapter,
            ds.base,
            ds.queries,
            args.dataset,
            workers=args.workers,
            duration_s=args.duration,
            ef_search=args.ef,
            writer_counts=writer_counts,
            batch_sizes=batch_sizes,
            mvcc=mvcc,
        )

    args.out.mkdir(parents=True, exist_ok=True)
    _write_csv(results, args.out / "contention_sweep.csv")
    log.info("wrote %s", args.out / "contention_sweep.csv")
    _log_before_after(results)
    return 0


def _log_before_after(results: list[BenchResult]) -> None:
    """Log a per-cell retained-ratio before/after table when both modes ran. The
    ratio is the honest signal on a shared dev box; absolute QPS is
    reference-hardware-pending (the dev box is WSL2 and the Python client is itself
    a concurrency ceiling)."""
    off = {(r.config["writers"], r.config["batch"]): r.config["ratio"] for r in results if not r.config.get("mvcc") and r.notes.startswith("read_during_write")}
    on = {(r.config["writers"], r.config["batch"]): r.config["ratio"] for r in results if r.config.get("mvcc") and r.notes.startswith("read_during_write")}
    if not off or not on:
        return
    log.info("retained read-QPS ratio — RwLock (off) vs MVCC (on), same box:")
    log.info("  writers  batch |   off  ->   on")
    for key in sorted(off):
        if key in on:
            nw, batch = key
            log.info("  %7d  %5d | %5.2fx -> %5.2fx", nw, batch, off[key], on[key])


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
