# SPDX-License-Identifier: AGPL-3.0-only
"""The benchmark runner: build a collection, sweep ``ef_search``, and report
recall@k, latency percentiles, and single-thread QPS.

Run against a live server (see ``bench/README.md``):

    uv run --project bench python -m quiver_bench.run --synthetic --api-key KEY
    uv run --project bench python -m quiver_bench.run --dataset bench/datasets/sift1m

Published figures must come from the documented reference hardware in
``docs/benchmarks/methodology.md`` — this dev box is resource-shared and is
**not** a source of official numbers. We never fabricate results.
"""

from __future__ import annotations

import argparse
import csv
import sys
import time
from pathlib import Path

from quiver import Client

from . import datasets
from .metrics import mean_recall_at_k, percentile


def _parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(prog="quiver-bench")
    src = p.add_mutually_exclusive_group(required=True)
    src.add_argument("--dataset", type=Path, help="directory with *_base/_query/_groundtruth files")
    src.add_argument("--synthetic", action="store_true", help="small random dataset (smoke run)")
    p.add_argument("--n", type=int, default=2000, help="synthetic base size")
    p.add_argument("--dim", type=int, default=16, help="synthetic dimensionality")
    p.add_argument("--queries", type=int, default=100, help="synthetic query count")
    p.add_argument("--url", default="http://127.0.0.1:6333")
    p.add_argument("--api-key", default=None)
    p.add_argument("--collection", default="bench")
    p.add_argument("--k", type=int, default=10)
    p.add_argument("--ef", default="32,64,128", help="comma-separated ef_search sweep")
    p.add_argument("--batch", type=int, default=500, help="upsert batch size")
    p.add_argument("--out", type=Path, default=None, help="write results CSV to this path")
    return p.parse_args(argv)


def _load(args: argparse.Namespace) -> datasets.Dataset:
    if args.synthetic:
        return datasets.synthetic(n=args.n, dim=args.dim, queries=args.queries, k=args.k)
    return datasets.load_sift(args.dataset, k=max(args.k, 100))


def _build(client: Client, name: str, ds: datasets.Dataset, batch: int) -> float:
    """(Re)create the collection and upsert the base vectors; return build seconds."""
    try:
        client.delete_collection(name)
    except Exception:  # noqa: BLE001 - best-effort reset
        pass
    client.create_collection(name, dim=int(ds.base.shape[1]), metric="l2")
    start = time.perf_counter()
    for lo in range(0, ds.base.shape[0], batch):
        chunk = ds.base[lo : lo + batch]
        points = [
            {"id": str(lo + j), "vector": vec.tolist()} for j, vec in enumerate(chunk)
        ]
        client.upsert(name, points)
    return time.perf_counter() - start


def _measure(client: Client, name: str, ds: datasets.Dataset, k: int, ef: int) -> dict:
    # Warm up (discarded) so the first-query JIT/allocation cost is excluded.
    for q in ds.queries[: min(10, ds.queries.shape[0])]:
        client.search(name, q.tolist(), k=k, ef_search=ef, with_payload=False)

    retrieved: list[list[int]] = []
    latencies_ms: list[float] = []
    for q in ds.queries:
        start = time.perf_counter()
        hits = client.search(name, q.tolist(), k=k, ef_search=ef, with_payload=False)
        latencies_ms.append((time.perf_counter() - start) * 1000.0)
        retrieved.append([int(h.id) for h in hits])

    truth = [row.tolist() for row in ds.ground_truth]
    total_s = sum(latencies_ms) / 1000.0
    return {
        "ef_search": ef,
        "recall@k": round(mean_recall_at_k(retrieved, truth, k), 4),
        "qps_1t": round(len(latencies_ms) / total_s, 1) if total_s > 0 else 0.0,
        "p50_ms": round(percentile(latencies_ms, 50), 3),
        "p95_ms": round(percentile(latencies_ms, 95), 3),
        "p99_ms": round(percentile(latencies_ms, 99), 3),
    }


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    ds = _load(args)
    ef_list = [int(x) for x in args.ef.split(",") if x.strip()]

    print(f"dataset: {ds.name}  queries={ds.queries.shape[0]}  k={args.k}")
    if args.synthetic:
        print("MODE: synthetic smoke run — NOT a published number.")

    with Client(args.url, api_key=args.api_key) as client:
        build_s = _build(client, args.collection, ds, args.batch)
        print(f"build: {build_s:.2f}s for {ds.base.shape[0]} vectors\n")
        rows = [{"build_s": round(build_s, 2), **_measure(client, args.collection, ds, args.k, ef)} for ef in ef_list]

    header = ["ef_search", "recall@k", "qps_1t", "p50_ms", "p95_ms", "p99_ms"]
    print("  ".join(f"{h:>9}" for h in header))
    for row in rows:
        print("  ".join(f"{row[h]:>9}" for h in header))

    print(
        "\nNote: official recall/QPS/RSS figures come from the documented "
        "reference hardware (docs/benchmarks/methodology.md), not this box."
    )

    if args.out is not None:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        with args.out.open("w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=["build_s", *header])
            writer.writeheader()
            writer.writerows(rows)
        print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
