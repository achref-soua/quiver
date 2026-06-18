# SPDX-License-Identifier: AGPL-3.0-only
"""Multi-DB comparison runner.

Orchestrates all competitor adapters against a shared dataset,
collecting BenchResults per competitor and writing them as CSV + JSON.

Usage (after starting a Quiver server on 127.0.0.1:6333):

    uv run --project bench python -m quiver_bench.comparison --smoke
    uv run --project bench python -m quiver_bench.comparison --dataset sift1m --competitors faiss,lancedb
    uv run --project bench python -m quiver_bench.comparison --dataset sift1m --competitors all
"""

from __future__ import annotations

import argparse
import csv
import json
import logging
import sys
import time
from pathlib import Path

import numpy as np

from . import datasets, manifest
from .competitors.base import BenchResult, CompetitorAdapter

log = logging.getLogger("quiver_bench.comparison")

# Default ef/nprobe sweep values (used when the competitor's param is ef_search)
DEFAULT_EF_SWEEP = [16, 32, 64, 128, 256]
DEFAULT_NPROBE_SWEEP = [4, 8, 16, 32, 64]

# Top-k for comparison
K = 10


def _all_adapters(quiver_url: str, quiver_key: str | None) -> dict[str, CompetitorAdapter]:
    """Return a mapping name → adapter for every competitor."""
    from .competitors.chroma_adapter import ChromaAdapter
    from .competitors.faiss_adapter import FaissAdapter
    from .competitors.lancedb_adapter import LanceDBAdapter
    from .competitors.milvus_lite_adapter import MilvusLiteAdapter
    from .competitors.pgvector_adapter import PgvectorAdapter
    from .competitors.qdrant_adapter import QdrantAdapter
    from .competitors.quiver_adapter import QuiverAdapter
    from .competitors.weaviate_adapter import WeaviateAdapter

    return {
        "quiver": QuiverAdapter(url=quiver_url, api_key=quiver_key),
        "faiss": FaissAdapter(),
        "lancedb": LanceDBAdapter(),
        "chroma": ChromaAdapter(),
        "milvus_lite": MilvusLiteAdapter(),
        "qdrant": QdrantAdapter(),
        "pgvector": PgvectorAdapter(),
        "weaviate": WeaviateAdapter(),
    }


def _load_dataset(name: str, datasets_dir: Path) -> datasets.Dataset:
    if name == "siftsmall":
        d = datasets_dir / "siftsmall"
        if d.exists():
            return datasets.load_sift(d, k=K)
        return datasets.synthetic(n=10_000, dim=128, queries=100, k=K)
    if name == "sift1m":
        d = datasets_dir / "sift"
        if not d.exists():
            raise FileNotFoundError(
                f"SIFT1M not found at {d}. "
                "Download from http://corpus-texmex.irisa.fr/ and extract to bench/datasets/sift/"
            )
        return datasets.load_sift(d, k=K)
    if name == "synthetic":
        return datasets.synthetic(n=2000, dim=16, queries=50, k=K)
    raise ValueError(f"Unknown dataset: {name!r}. Use siftsmall, sift1m, or synthetic.")


def run_one(
    adapter: CompetitorAdapter,
    ds: datasets.Dataset,
    dataset_name: str,
    out_dir: Path,
    ef_sweep: list[int],
    dry_run: bool = False,
) -> list[BenchResult] | None:
    """Run a single competitor and write its CSV; returns results or None on failure."""
    out_dir.mkdir(parents=True, exist_ok=True)
    csv_path = out_dir / f"{adapter.name}.csv"

    log.info("[%s] starting ...", adapter.name)
    try:
        adapter.start()
    except Exception as exc:  # noqa: BLE001
        log.warning("[%s] start failed: %s — skipping", adapter.name, exc)
        return None

    results: list[BenchResult] = []
    try:
        log.info("[%s] building index (%d vectors) ...", adapter.name, ds.base.shape[0])
        build_s, disk_mb = adapter.build(ds.base, metric="l2")
        log.info("[%s] build done in %.1fs", adapter.name, build_s)

        if not dry_run:
            sweep = ef_sweep if adapter.param_name == "ef_search" else DEFAULT_NPROBE_SWEEP
            results = adapter.query_sweep(ds.queries, ds.ground_truth, K, sweep, reps=3)
            for r in results:
                r.dataset = dataset_name
                r.build_s = build_s
                r.index_disk_mb = disk_mb

            _write_csv(results, csv_path)
            log.info("[%s] wrote %s", adapter.name, csv_path)
    except Exception as exc:  # noqa: BLE001
        log.error("[%s] error during benchmark: %s", adapter.name, exc)
        results = [
            BenchResult(
                competitor=adapter.name,
                dataset=dataset_name,
                param_name=adapter.param_name,
                param_value=0,
                error=str(exc),
            )
        ]
    finally:
        try:
            adapter.stop()
        except Exception:  # noqa: BLE001
            pass

    return results


def _write_csv(results: list[BenchResult], path: Path) -> None:
    if not results:
        return
    fields = list(results[0].as_dict().keys())
    with path.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        w.writerows(r.as_dict() for r in results)


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")

    p = argparse.ArgumentParser(prog="quiver_bench.comparison")
    p.add_argument(
        "--dataset",
        default="siftsmall",
        choices=["siftsmall", "sift1m", "synthetic"],
        help="Dataset to use (default: siftsmall smoke run)",
    )
    p.add_argument("--smoke", action="store_true", help="Alias for --dataset siftsmall")
    p.add_argument(
        "--competitors",
        default="all",
        help="Comma-separated list (faiss,lancedb,chroma,milvus_lite,qdrant,pgvector,weaviate,quiver) or 'all'",
    )
    p.add_argument("--quiver-url", default="http://127.0.0.1:6333")
    p.add_argument("--quiver-key", default=None)
    p.add_argument(
        "--out",
        type=Path,
        default=Path("docs/benchmarks/results/comparison-v0.17.0"),
        help="Output directory",
    )
    p.add_argument("--datasets-dir", type=Path, default=Path("bench/datasets"))
    p.add_argument("--ef", default="16,32,64,128,256", help="ef_search sweep (comma-separated)")
    p.add_argument("--dry-run", action="store_true", help="Start/stop each adapter but skip query sweep")
    args = p.parse_args(sys.argv[1:] if argv is None else argv)

    if args.smoke:
        args.dataset = "siftsmall"

    ef_sweep = [int(x) for x in args.ef.split(",") if x.strip()]
    dataset_name = args.dataset

    # Dataset sub-dir within the output
    if dataset_name == "siftsmall":
        out_dir = args.out / "smoke"
    else:
        out_dir = args.out / dataset_name
    out_dir.mkdir(parents=True, exist_ok=True)

    # Manifest
    hw = manifest.capture()
    manifest.write(args.out, hw)
    log.info("Manifest written to %s/manifest.json", args.out)
    log.info("Hardware: %s / %dGB RAM", hw.get("processor", "unknown"), int((hw.get("ram_total_mb") or 0) / 1024))

    # Load dataset
    try:
        ds = _load_dataset(dataset_name, args.datasets_dir)
    except FileNotFoundError as exc:
        log.error("%s", exc)
        return 1
    log.info("Dataset: %s  n=%d  dim=%d  queries=%d", dataset_name, ds.base.shape[0], ds.base.shape[1], ds.queries.shape[0])

    # Select competitors
    all_adapters = _all_adapters(args.quiver_url, args.quiver_key)
    if args.competitors == "all":
        selected = list(all_adapters.keys())
    else:
        selected = [s.strip() for s in args.competitors.split(",") if s.strip()]
        unknown = [s for s in selected if s not in all_adapters]
        if unknown:
            log.error("Unknown competitors: %s", unknown)
            return 1

    all_results: list[BenchResult] = []
    for name in selected:
        adapter = all_adapters[name]
        log.info("=== %s ===", name)
        t0 = time.perf_counter()
        results = run_one(adapter, ds, dataset_name, out_dir, ef_sweep, dry_run=args.dry_run)
        elapsed = time.perf_counter() - t0
        if results:
            all_results.extend(results)
            log.info("[%s] done in %.1fs", name, elapsed)
        else:
            log.warning("[%s] skipped or failed", name)

    # Write a combined JSON summary
    summary_path = out_dir / "summary.json"
    summary_path.write_text(json.dumps([r.as_dict() for r in all_results], indent=2))
    log.info("Summary written to %s", summary_path)

    print(f"\nResults in {out_dir}/  —  run `just bench-report` to generate the comparison report.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
