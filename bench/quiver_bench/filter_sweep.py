# SPDX-License-Identifier: AGPL-3.0-only
"""Filtered-selectivity sweep — Quiver only.

Measures how recall and throughput move as a payload pre-filter gets more
selective. Each base point gets a ``bucket = id % 100`` payload, so the filter
``bucket < s`` matches exactly ``s``% of the collection. For every selectivity
we recompute the *filtered* exact ground truth by brute force over the matching
subset (the only honest recall reference under a filter) and compare the engine's
filtered results against it.

Run against an already-running server::

    uv run --project bench python -m quiver_bench.filter_sweep \
        --dataset siftsmall --out docs/benchmarks/results/comparison-v0.22.0
"""

from __future__ import annotations

import argparse
import logging
import statistics
import sys
import time
from pathlib import Path

import numpy as np

from . import manifest
from .comparison import _load_dataset, _write_csv
from .competitors.base import BenchResult
from .competitors.quiver_adapter import COLLECTION
from .metrics import mean_recall_at_k, percentile

log = logging.getLogger("quiver_bench.filter_sweep")

K = 10
DEFAULT_SELECTIVITIES = [1, 5, 25, 50, 100]  # percent of the collection kept
BUCKETS = 100


def selectivity_mask(n: int, pct: int) -> np.ndarray:
    """Boolean mask over ``n`` rows keeping ``pct``% of them via ``id % 100``.

    Row ``i`` is kept when ``i % 100 < pct``; with a uniform id distribution this
    keeps ~``pct``% of the rows and exactly matches the engine's ``bucket < pct``
    filter, so the brute-force truth and the engine see the *same* subset.
    """
    return (np.arange(n) % BUCKETS) < pct


def filtered_truth(base: np.ndarray, queries: np.ndarray, mask: np.ndarray, k: int) -> list[list[int]]:
    """Exact top-``k`` L2 neighbours within the masked subset, as base-row ids."""
    idx = np.nonzero(mask)[0]
    if idx.size == 0:
        return [[] for _ in queries]
    sub = base[idx]
    sub_sq = np.einsum("ij,ij->i", sub, sub)
    kk = min(k, idx.size)
    out: list[list[int]] = []
    for q in queries:
        dist = sub_sq - 2.0 * (sub @ q)
        top = np.argpartition(dist, kk - 1)[:kk]
        top = top[np.argsort(dist[top])]
        out.append([int(idx[t]) for t in top])
    return out


def run_filter_sweep(
    quiver_url: str,
    quiver_key: str | None,
    dataset_name: str,
    datasets_dir: Path,
    out_dir: Path,
    selectivities: list[int],
    ef_search: int = 128,
    reps: int = 3,
) -> list[BenchResult]:
    """Build a filterable collection and sweep the filter selectivity."""
    from quiver import Client, FilterableField  # type: ignore[import]

    ds = _load_dataset(dataset_name, datasets_dir)
    base, queries = ds.base, ds.queries
    n, dim = int(base.shape[0]), int(base.shape[1])
    log.info("filter sweep: %s n=%d dim=%d selectivities=%s", dataset_name, n, dim, selectivities)

    client = Client(quiver_url, api_key=quiver_key, timeout=3600.0)
    try:
        client.delete_collection(COLLECTION)
    except Exception:  # noqa: BLE001
        pass
    client.create_collection(
        COLLECTION, dim=dim, metric="l2", filterable=[FilterableField("bucket", "numeric")]
    )

    # Bulk-ingest with a bucket payload, then force the deferred index build.
    from .competitors.quiver_adapter import bulk_batch_size

    batch = bulk_batch_size(dim)
    for lo in range(0, n, batch):
        chunk = base[lo : lo + batch]
        points = [
            {"id": str(lo + j), "vector": vec.tolist(), "payload": {"bucket": (lo + j) % BUCKETS}}
            for j, vec in enumerate(chunk)
        ]
        client._send("POST", f"/v1/collections/{COLLECTION}/points:bulk", {"points": points})
    client.search(COLLECTION, base[0].tolist(), k=K, ef_search=ef_search, with_payload=False)

    out_dir.mkdir(parents=True, exist_ok=True)
    results: list[BenchResult] = []
    for pct in selectivities:
        truth = filtered_truth(base, queries, selectivity_mask(n, pct), K)
        flt = None if pct >= BUCKETS else {"lt": {"field": "bucket", "value": pct}}
        rep_recall: list[float] = []
        rep_qps: list[float] = []
        latencies: list[float] = []
        for _ in range(reps):
            retrieved: list[list[int]] = []
            lat: list[float] = []
            for q in queries:
                t0 = time.perf_counter()
                hits = client.search(
                    COLLECTION, q.tolist(), k=K, ef_search=ef_search, filter=flt, with_payload=False
                )
                lat.append((time.perf_counter() - t0) * 1000.0)
                retrieved.append([int(h.id) for h in hits])
            total_s = sum(lat) / 1000.0
            rep_recall.append(mean_recall_at_k(retrieved, truth, K))
            rep_qps.append(len(lat) / total_s if total_s > 0 else 0.0)
            latencies.extend(lat)
        results.append(
            BenchResult(
                competitor="quiver",
                dataset=dataset_name,
                param_name="selectivity_pct",
                param_value=pct,
                recall_at_10=round(statistics.mean(rep_recall), 4),
                qps_1t=round(statistics.mean(rep_qps), 1),
                p50_ms=round(percentile(latencies, 50), 3),
                p95_ms=round(percentile(latencies, 95), 3),
                p99_ms=round(percentile(latencies, 99), 3),
                notes=f"sel={pct}%",
                config={"selectivity_pct": pct, "ef_search": ef_search},
            )
        )
        log.info("[sel=%d%%] recall@10=%.4f qps=%.1f", pct, results[-1].recall_at_10, results[-1].qps_1t)

    client.close()
    _write_csv(results, out_dir / "filter_sweep.csv")
    log.info("wrote %s", out_dir / "filter_sweep.csv")
    return results


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    p = argparse.ArgumentParser(prog="quiver_bench.filter_sweep")
    p.add_argument("--dataset", default="siftsmall")
    p.add_argument("--quiver-url", default="http://127.0.0.1:6333")
    p.add_argument("--quiver-key", default=None)
    p.add_argument("--out", type=Path, default=Path("docs/benchmarks/results/comparison-v0.22.0"))
    p.add_argument("--datasets-dir", type=Path, default=Path("bench/datasets"))
    p.add_argument("--ef", type=int, default=128)
    p.add_argument("--selectivities", default="1,5,25,50,100")
    args = p.parse_args(sys.argv[1:] if argv is None else argv)

    sels = [int(x) for x in args.selectivities.split(",") if x.strip()]
    # Match comparison.py's sub-dir convention (siftsmall → smoke/).
    out_dir = args.out / ("smoke" if args.dataset == "siftsmall" else args.dataset)
    manifest.write(args.out, manifest.capture())
    run_filter_sweep(
        args.quiver_url, args.quiver_key, args.dataset, args.datasets_dir, out_dir, sels, args.ef
    )
    print(f"\nFilter sweep in {out_dir}/filter_sweep.csv — run `just bench-report` to fold it in.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
