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

Run against a freshly built server (the adapter starts one)::

    uv run --project bench python -m quiver_bench.contention_sweep \
        --dataset sift1m --workers 8 --duration 10 \
        --out docs/benchmarks/results/comparison-v0.23.0

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
) -> tuple[int, int, float]:
    """Run `workers` reader threads (and one writer thread if `write_fn`) in a
    busy loop for `duration_s`, returning `(total_reads, total_writes, elapsed_s)`.

    Pure given its callables, so it is unit-tested with fakes — no server. The
    reads/writes are HTTP calls that release the GIL, so the server-side
    read/write concurrency is what is actually exercised."""
    stop = threading.Event()
    reads = [0] * workers
    writes = [0]

    def reader(slot: int) -> None:
        n = 0
        while not stop.is_set():
            read_fn()
            n += 1
        reads[slot] = n

    def writer() -> None:
        n = 0
        while not stop.is_set():
            write_fn()  # type: ignore[misc]
            n += 1
        writes[0] = n

    threads = [threading.Thread(target=reader, args=(i,)) for i in range(workers)]
    if write_fn is not None:
        threads.append(threading.Thread(target=writer))

    t0 = time.perf_counter()
    for t in threads:
        t.start()
    # Sleep the wall-clock window, then signal stop and drain.
    time.sleep(duration_s)
    stop.set()
    for t in threads:
        t.join()
    return sum(reads), writes[0], time.perf_counter() - t0


def run_contention(
    adapter: QuiverAdapter,
    base,
    queries,
    dataset_name: str,
    *,
    workers: int,
    duration_s: float,
    ef_search: int,
) -> list[BenchResult]:
    """Build, then measure read QPS read-only and under a concurrent writer."""
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

        def write_fn() -> None:
            i = int(rng.integers(0, n))
            adapter.write_one(str(i), base[i])

        # Phase 1: read-only baseline.
        r0, _, e0 = _drive(read_fn, workers, duration_s)
        read_only_qps = r0 / e0 if e0 > 0 else 0.0
        # Phase 2: same readers, plus a concurrent writer.
        r1, w1, e1 = _drive(read_fn, workers, duration_s, write_fn)
        under_write_qps = r1 / e1 if e1 > 0 else 0.0

        ratio = under_write_qps / read_only_qps if read_only_qps > 0 else 0.0
        log.info(
            "read-only %.0f QPS | under-write %.0f QPS (%.0f writes) | retained %.2fx",
            read_only_qps,
            under_write_qps,
            w1 / e1 if e1 > 0 else 0.0,
            ratio,
        )

        def result(notes: str, qps: float) -> BenchResult:
            r = BenchResult(
                competitor="quiver",
                dataset=dataset_name,
                param_name="ef_search",
                param_value=ef_search,
                qps_nt=qps,
                concurrency=workers,
                build_s=build_s,
                notes=notes,
            )
            return r

        return [
            result("read_only", read_only_qps),
            result("read_during_write", under_write_qps),
        ]
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
    p.add_argument("--duration", type=float, default=10.0, help="seconds per phase")
    p.add_argument("--ef", type=int, default=64)
    args = p.parse_args(argv)

    ds = _load_dataset(args.dataset, args.datasets_dir)
    adapter = QuiverAdapter(url=args.quiver_url, api_key=args.quiver_key, start_server=True)
    results = run_contention(
        adapter,
        ds.base,
        ds.queries,
        args.dataset,
        workers=args.workers,
        duration_s=args.duration,
        ef_search=args.ef,
    )
    args.out.mkdir(parents=True, exist_ok=True)
    _write_csv(results, args.out / "contention_sweep.csv")
    log.info("wrote %s", args.out / "contention_sweep.csv")
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
