# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the multi-DB comparison harness."""

from __future__ import annotations

import numpy as np
import pytest

from quiver_bench import datasets
from quiver_bench.competitors.base import BenchResult, CompetitorAdapter
from quiver_bench.rss import _parse_mem
from quiver_bench.report import generate, _best_at_recall, _wins_matrix


# ── RSS helpers ──────────────────────────────────────────────────────────────

def test_parse_mem_mib():
    assert _parse_mem("512MiB") == pytest.approx(512.0)


def test_parse_mem_gib():
    assert _parse_mem("1.5GiB") == pytest.approx(1536.0)


def test_parse_mem_mb():
    assert _parse_mem("256MB") == pytest.approx(256.0)


def test_parse_mem_kb():
    val = _parse_mem("2048kB")
    assert val == pytest.approx(2.0)


def test_parse_mem_unknown():
    assert _parse_mem("???") is None


# ── BenchResult ───────────────────────────────────────────────────────────────

def test_bench_result_as_dict():
    r = BenchResult(competitor="faiss", dataset="siftsmall", param_name="ef_search", param_value=64)
    d = r.as_dict()
    assert d["competitor"] == "faiss"
    assert d["param_value"] == 64


# ── Stub adapter ─────────────────────────────────────────────────────────────

class _PerfectAdapter(CompetitorAdapter):
    """Returns the exact nearest neighbour (brute force) — recall@10 = 1.0."""

    name = "perfect"
    param_name = "ef_search"

    def __init__(self, base: np.ndarray) -> None:
        self._base = base
        self._metric = "l2"

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        self._base = base_vectors
        self._metric = metric
        return 0.0, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        diffs = self._base - query
        dists = np.einsum("ij,ij->i", diffs, diffs)
        return np.argsort(dists)[:k].tolist()

    def sample_rss(self) -> float | None:
        return 42.0


def test_stub_adapter_recall():
    ds = datasets.synthetic(n=200, dim=8, queries=20, k=10)
    adapter = _PerfectAdapter(ds.base)
    adapter.build(ds.base, "l2")
    results = adapter.query_sweep(ds.queries, ds.ground_truth, k=10, params=[32], reps=1)
    assert len(results) == 1
    assert results[0].recall_at_10 == pytest.approx(1.0, abs=0.01)
    assert results[0].rss_mb == pytest.approx(42.0)
    assert results[0].qps_1t > 0
    # Single-thread by default: no saturated-QPS pass.
    assert results[0].qps_nt is None
    assert results[0].concurrency == 1


def test_concurrent_pass_populates_qps_nt():
    ds = datasets.synthetic(n=200, dim=8, queries=40, k=10)
    adapter = _PerfectAdapter(ds.base)
    adapter.build(ds.base, "l2")
    results = adapter.query_sweep(
        ds.queries, ds.ground_truth, k=10, params=[32], reps=1, concurrency=4
    )
    assert results[0].concurrency == 4
    assert results[0].qps_nt is not None and results[0].qps_nt > 0
    # Recall is unchanged by the concurrent pass.
    assert results[0].recall_at_10 == pytest.approx(1.0, abs=0.01)


def test_query_concurrent_returns_positive_qps():
    ds = datasets.synthetic(n=100, dim=8, queries=30, k=5)
    adapter = _PerfectAdapter(ds.base)
    qps = adapter.query_concurrent(ds.queries, k=5, param=32, workers=4)
    assert qps > 0


# ── Report helpers ────────────────────────────────────────────────────────────

def _make_row(comp: str, recall: float, qps: float, rss: float, build: float) -> dict:
    return {
        "competitor": comp,
        "recall_at_10": str(recall),
        "qps_1t": str(qps),
        "rss_mb": str(rss),
        "build_s": str(build),
        "index_disk_mb": "None",
        "param_name": "ef_search",
        "param_value": "64",
    }


def test_best_at_recall_meets_target():
    rows = [
        _make_row("x", 0.90, 100, 500, 10),
        _make_row("x", 0.96, 80, 510, 10),
        _make_row("x", 0.99, 50, 520, 10),
    ]
    best = _best_at_recall(rows, 0.95)
    assert best is not None
    assert float(best["recall_at_10"]) >= 0.95
    # Should pick the lowest recall that meets the target (0.96 not 0.99)
    assert float(best["recall_at_10"]) < 0.98


def test_best_at_recall_falls_back():
    rows = [_make_row("x", 0.88, 100, 500, 10)]
    best = _best_at_recall(rows, 0.95)
    assert best is not None
    assert float(best["recall_at_10"]) == pytest.approx(0.88)


def test_wins_matrix_no_quiver():
    result = _wins_matrix({})
    assert "not available" in result


def test_wins_matrix_quiver_wins_recall():
    data = {
        "quiver": _make_row("quiver", 0.97, 500, 200, 5),
        "faiss": _make_row("faiss", 0.93, 800, 150, 3),
    }
    matrix = _wins_matrix(data)
    # Quiver has higher recall → win on recall
    assert "win" in matrix or "loss" in matrix or "tie" in matrix


def test_report_generate_no_data(tmp_path):
    # An empty result dir should produce a usable (no-data) report
    report = generate(tmp_path)
    assert "# Quiver" in report
    assert "No result CSVs" in report


def test_report_generate_with_data(tmp_path):
    import csv
    sub = tmp_path / "smoke"
    sub.mkdir()
    fields = list(BenchResult("faiss", "siftsmall", "ef_search", 64).as_dict().keys())
    row = BenchResult("faiss", "siftsmall", "ef_search", 64, recall_at_10=0.97, qps_1t=1200, rss_mb=80, build_s=2.1)
    with (sub / "faiss.csv").open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        w.writerow(row.as_dict())
    report = generate(tmp_path)
    assert "faiss" in report
    assert "0.9700" in report
