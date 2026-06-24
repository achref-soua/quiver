# SPDX-License-Identifier: AGPL-3.0-only
"""Quantization memory-wedge sweep — Quiver only.

Builds the *same* dataset under several Quiver index/quantization configs and
records recall@{1,10,100} against the resident memory each one needs. This is
Quiver's headline tradeoff (the "memory wedge"): exact in-memory HNSW gives the
best recall at the highest RAM, while IVF+PQ and the disk-resident Vamana graph
trade a little recall for a much smaller footprint.

Run against an already-running server (see ``scripts/run-comparison.sh``)::

    uv run --project bench python -m quiver_bench.quant_sweep \
        --dataset sift1m --out docs/benchmarks/results/comparison-v0.22.0

Absolute RSS on a shared dev box is indicative, not authoritative — the
reference-hardware figure stays pending (we never fabricate it).
"""

from __future__ import annotations

import argparse
import logging
import sys
import time
from pathlib import Path

from . import manifest
from .comparison import _load_dataset, _write_csv
from .competitors.base import BenchResult
from .competitors.quiver_adapter import QuiverAdapter

log = logging.getLogger("quiver_bench.quant_sweep")

K = 10


def default_configs(dim: int) -> list[dict]:
    """The memory-wedge configs for a ``dim``-d dataset.

    PQ uses ``dim // 8`` subspaces (one byte per subspace ≈ 32× smaller than raw
    f32 vectors) when that divides the dimension evenly, else the engine default.
    """
    m = max(1, dim // 8)
    pq = m if dim % m == 0 else None
    return [
        {"index": "hnsw"},  # exact vectors in RAM — recall ceiling, RAM ceiling
        {"index": "ivf", "pq_subspaces": pq},  # PQ codes in RAM
        {"index": "disk_vamana", "pq_subspaces": pq},  # PQ in RAM, vectors on SSD
    ]


def select_configs(dim: int, indexes: list[str] | None) -> list[dict]:
    """Filter :func:`default_configs` to the named index kinds (order preserved)."""
    configs = default_configs(dim)
    if not indexes:
        return configs
    return [c for c in configs if (c["index"] in indexes)]


def run_quant_sweep(
    quiver_url: str,
    quiver_key: str | None,
    dataset_name: str,
    datasets_dir: Path,
    out_dir: Path,
    ef_sweep: list[int],
    configs: list[dict] | None = None,
    *,
    start_server: bool = False,
) -> list[BenchResult]:
    """Build each config and sweep it; write ``quant_sweep.csv``.

    With ``start_server`` each config runs against a **fresh** Quiver process and
    data dir, so its measured RSS is its own — not a shared server's cumulative
    high-water mark (the allocator does not return freed pages, so a reused
    process reports a meaningless monotonic RSS for the wedge)."""
    ds = _load_dataset(dataset_name, datasets_dir)
    dim = int(ds.base.shape[1])
    configs = configs or default_configs(dim)
    log.info("memory wedge: %d configs over %s (dim=%d)", len(configs), dataset_name, dim)

    out_dir.mkdir(parents=True, exist_ok=True)
    results: list[BenchResult] = []
    for cfg in configs:
        adapter = QuiverAdapter(
            url=quiver_url,
            api_key=quiver_key,
            index=cfg.get("index"),
            pq_subspaces=cfg.get("pq_subspaces"),
            start_server=start_server,
        )
        label = adapter.config_label
        log.info("[%s] building ...", label)
        adapter.start()
        try:
            t0 = time.perf_counter()
            build_s, disk_mb = adapter.build(ds.base, metric="l2")
            log.info("[%s] built in %.1fs", label, build_s)
            sweep = adapter.query_sweep(ds.queries, ds.ground_truth, K, ef_sweep, reps=3)
            for r in sweep:
                r.dataset = dataset_name
                r.build_s = build_s
                r.index_disk_mb = disk_mb
                r.config = {"index": cfg.get("index") or "hnsw", "pq_subspaces": cfg.get("pq_subspaces")}
                r.notes = label
            results.extend(sweep)
            log.info("[%s] done in %.1fs", label, time.perf_counter() - t0)
        except Exception as exc:  # noqa: BLE001
            log.error("[%s] failed: %s", label, exc)
            results.append(
                BenchResult(
                    competitor="quiver",
                    dataset=dataset_name,
                    param_name="ef_search",
                    param_value=0,
                    notes=label,
                    error=str(exc),
                )
            )
        finally:
            adapter.stop()

    _write_csv(results, out_dir / "quant_sweep.csv")
    log.info("wrote %s", out_dir / "quant_sweep.csv")
    return results


def main(argv: list[str] | None = None) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    p = argparse.ArgumentParser(prog="quiver_bench.quant_sweep")
    p.add_argument("--dataset", default="siftsmall")
    p.add_argument("--quiver-url", default="http://127.0.0.1:6333")
    p.add_argument("--quiver-key", default=None)
    p.add_argument("--out", type=Path, default=Path("docs/benchmarks/results/comparison-v0.22.0"))
    p.add_argument("--datasets-dir", type=Path, default=Path("bench/datasets"))
    p.add_argument("--ef", default="64,128", help="ef_search sweep (comma-separated)")
    p.add_argument(
        "--indexes",
        default=None,
        help="Comma-separated index kinds to include (default: hnsw,ivf,disk_vamana)",
    )
    p.add_argument(
        "--start-server",
        action="store_true",
        help="Spawn a fresh Quiver process per config (isolated, honest per-config RSS)",
    )
    args = p.parse_args(sys.argv[1:] if argv is None else argv)

    ef_sweep = [int(x) for x in args.ef.split(",") if x.strip()]
    indexes = [s.strip() for s in args.indexes.split(",")] if args.indexes else None
    # Match comparison.py's sub-dir convention so the report folds sweeps and the
    # competitor matrix into the same per-dataset section.
    out_dir = args.out / ("smoke" if args.dataset == "siftsmall" else args.dataset)
    manifest.write(args.out, manifest.capture())
    ds_dim = _load_dataset(args.dataset, args.datasets_dir).base.shape[1]
    run_quant_sweep(
        args.quiver_url,
        args.quiver_key,
        args.dataset,
        args.datasets_dir,
        out_dir,
        ef_sweep,
        configs=select_configs(int(ds_dim), indexes),
        start_server=args.start_server,
    )
    print(f"\nMemory wedge in {out_dir}/quant_sweep.csv — run `just bench-report` to fold it in.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
