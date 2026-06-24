# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the v0.22.0 benchmark dimensions: recall@{1,100}, the qdrant
thread-local client, the quantization memory-wedge configs, and the
filtered-selectivity helpers + their report sections."""

from __future__ import annotations

import sys
import threading
import types

import numpy as np
import pytest

from quiver_bench import datasets
from quiver_bench.competitors.base import CompetitorAdapter


# ── recall@{1,10,100} in the sweep ───────────────────────────────────────────

class _PerfectAdapter(CompetitorAdapter):
    name = "perfect"
    param_name = "ef_search"

    def __init__(self, base: np.ndarray) -> None:
        self._base = base

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        self._base = base_vectors
        return 0.0, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        diffs = self._base - query
        dists = np.einsum("ij,ij->i", diffs, diffs)
        return np.argsort(dists)[:k].tolist()


def test_sweep_reports_all_three_recall_levels():
    ds = datasets.synthetic(n=300, dim=8, queries=20, k=10)
    adapter = _PerfectAdapter(ds.base)
    results = adapter.query_sweep(ds.queries, ds.ground_truth, k=10, params=[32], reps=1)
    r = results[0]
    # Exact adapter ⇒ perfect recall at every depth.
    assert r.recall_at_1 == pytest.approx(1.0, abs=0.01)
    assert r.recall_at_10 == pytest.approx(1.0, abs=0.01)
    assert r.recall_at_100 == pytest.approx(1.0, abs=0.01)


class _CountingAdapter(_PerfectAdapter):
    """Counts query depth requested, to prove the deep recall@100 pass runs."""

    def __init__(self, base: np.ndarray) -> None:
        super().__init__(base)
        self.ks_seen: list[int] = []

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        self.ks_seen.append(k)
        return super().query_one(query, k, param)


def test_deep_pass_retrieves_100_for_recall_at_100():
    ds = datasets.synthetic(n=300, dim=8, queries=5, k=10)
    adapter = _CountingAdapter(ds.base)
    adapter.query_sweep(ds.queries, ds.ground_truth, k=10, params=[32], reps=1)
    # The timed loop runs at k=10; one extra untimed pass runs at k=100.
    assert 10 in adapter.ks_seen
    assert 100 in adapter.ks_seen


# ── qdrant thread-local client ────────────────────────────────────────────────

def test_qdrant_uses_a_distinct_client_per_thread(monkeypatch):
    created: list[int] = []

    class _FakeClient:
        def __init__(self, url: str) -> None:
            created.append(id(self))

    fake_mod = types.ModuleType("qdrant_client")
    fake_mod.QdrantClient = _FakeClient
    monkeypatch.setitem(sys.modules, "qdrant_client", fake_mod)

    from quiver_bench.competitors.qdrant_adapter import QdrantAdapter

    adapter = QdrantAdapter()
    main_client = adapter._thread_client()
    # Same thread → cached, no second construction.
    assert adapter._thread_client() is main_client

    worker_client: list = []
    t = threading.Thread(target=lambda: worker_client.append(adapter._thread_client()))
    t.start()
    t.join()

    # The worker got its own client, distinct from the main thread's.
    assert worker_client[0] is not main_client
    assert len(created) == 2


# ── quantization memory-wedge configs ─────────────────────────────────────────

def test_default_configs_pq_divides_even_dim():
    from quiver_bench.quant_sweep import default_configs

    cfgs = default_configs(128)
    kinds = [c["index"] for c in cfgs]
    assert kinds == ["hnsw", "ivf", "disk_vamana"]
    # 128 // 8 = 16 and 128 % 16 == 0 → PQ uses 16 subspaces.
    assert cfgs[1]["pq_subspaces"] == 16


def test_default_configs_uneven_dim_falls_back_to_engine_default():
    from quiver_bench.quant_sweep import default_configs

    # 130 // 8 = 16 but 130 % 16 != 0 → let the engine pick (None).
    cfgs = default_configs(130)
    assert cfgs[1]["pq_subspaces"] is None


def test_select_configs_filters_by_index_kind():
    from quiver_bench.quant_sweep import select_configs

    # No filter → all three configs, order preserved.
    assert [c["index"] for c in select_configs(128, None)] == ["hnsw", "ivf", "disk_vamana"]
    # The clean 2-point wedge (exact-in-RAM vs PQ-codes-in-RAM), IVF dropped.
    assert [c["index"] for c in select_configs(128, ["hnsw", "disk_vamana"])] == [
        "hnsw",
        "disk_vamana",
    ]


def test_quiver_adapter_config_label():
    from quiver_bench.competitors.quiver_adapter import QuiverAdapter

    assert QuiverAdapter().config_label == "hnsw"
    assert QuiverAdapter(index="ivf", pq_subspaces=16).config_label == "ivf+pq16"
    assert QuiverAdapter(index="disk_vamana").config_label == "disk_vamana"


# ── filtered-selectivity helpers ──────────────────────────────────────────────

def test_selectivity_mask_keeps_the_right_fraction():
    from quiver_bench.filter_sweep import selectivity_mask

    mask = selectivity_mask(1000, 25)
    # id % 100 < 25 ⇒ exactly 25% kept on a clean multiple of 100.
    assert mask.sum() == 250
    assert selectivity_mask(1000, 100).all()


def test_filtered_truth_only_returns_allowed_rows_and_is_exact():
    from quiver_bench.filter_sweep import filtered_truth, selectivity_mask

    # Points on a line; query near index 0. Keep only ids where id%100 < 50.
    base = np.arange(200, dtype=np.float32).reshape(-1, 1)
    queries = np.array([[0.0]], dtype=np.float32)
    mask = selectivity_mask(200, 50)
    truth = filtered_truth(base, queries, mask, k=3)
    # Every returned id must satisfy the filter and be sorted nearest-first.
    assert all(i % 100 < 50 for i in truth[0])
    assert truth[0] == [0, 1, 2]


def test_filtered_truth_empty_mask():
    from quiver_bench.filter_sweep import filtered_truth

    base = np.arange(10, dtype=np.float32).reshape(-1, 1)
    queries = np.array([[0.0]], dtype=np.float32)
    mask = np.zeros(10, dtype=bool)
    assert filtered_truth(base, queries, mask, k=3) == [[]]


# ── report sections ───────────────────────────────────────────────────────────

def test_memory_wedge_section_renders(tmp_path):
    from quiver_bench.report import _memory_wedge_section

    (tmp_path / "quant_sweep.csv").write_text(
        "competitor,notes,param_value,recall_at_1,recall_at_10,recall_at_100,rss_mb,index_disk_mb,build_s\n"
        "quiver,hnsw,128,0.99,0.98,0.97,512,,10.0\n"
        "quiver,ivf+pq16,128,0.90,0.88,0.85,120,,40.0\n"
        "quiver,disk_vamana+pq16,128,0.85,0.80,0.78,64,200.0,60.0\n"
    )
    out = "\n".join(_memory_wedge_section(tmp_path))
    assert "Memory wedge" in out
    # Wedge order: hnsw before ivf before disk_vamana.
    assert out.index("hnsw") < out.index("ivf+pq16") < out.index("disk_vamana+pq16")


def test_filter_sweep_section_renders(tmp_path):
    from quiver_bench.report import _filter_sweep_section

    (tmp_path / "filter_sweep.csv").write_text(
        "competitor,param_value,recall_at_10,qps_1t,p50_ms,p95_ms,p99_ms\n"
        "quiver,1,0.99,2000,0.4,0.6,0.8\n"
        "quiver,50,0.97,800,1.0,1.5,2.0\n"
    )
    out = "\n".join(_filter_sweep_section(tmp_path))
    assert "Filtered-selectivity sweep" in out
    assert "1%" in out and "50%" in out


def test_footer_is_quiver_only_when_no_competitors(tmp_path):
    from quiver_bench.report import generate

    ds = tmp_path / "sift1m"
    ds.mkdir()
    (ds / "quiver.csv").write_text(
        "competitor,dataset,param_name,param_value,recall_at_10,qps_1t\n"
        "quiver,sift1m,ef_search,64,0.96,800\n"
    )
    out = generate(tmp_path)
    assert "Quiver-only" in out
    # The multi-DB note (which names competitors that did not run) must not appear.
    assert "Milvus is benchmarked" not in out


def test_footer_is_multidb_when_competitors_present(tmp_path):
    from quiver_bench.report import generate

    ds = tmp_path / "sift1m"
    ds.mkdir()
    (ds / "quiver.csv").write_text("competitor,recall_at_10,qps_1t\nquiver,0.96,800\n")
    (ds / "faiss.csv").write_text("competitor,recall_at_10,qps_1t\nfaiss,0.95,3000\n")
    out = generate(tmp_path)
    assert "Milvus is benchmarked" in out


def test_sweep_files_excluded_from_competitor_matrix(tmp_path):
    from quiver_bench.report import _load_results

    (tmp_path / "faiss.csv").write_text(
        "competitor,recall_at_10,qps_1t\nfaiss,0.95,800\n"
    )
    (tmp_path / "quant_sweep.csv").write_text(
        "competitor,notes,recall_at_10\nquiver,hnsw,0.98\n"
    )
    data = _load_results(tmp_path)
    # The quant sweep must NOT show up as a quiver competitor entry.
    assert set(data) == {"faiss"}


# ── cold-reopen RSS hook (ADR-0063) ──────────────────────────────────────────

def test_base_cold_reopen_is_a_noop():
    # The default hook is callable and does nothing — competitors that do not own
    # their server process inherit it unchanged.
    adapter = _PerfectAdapter(np.zeros((1, 4), dtype=np.float32))
    assert adapter.cold_reopen() is None


def test_quiver_cold_reopen_noop_without_managed_server():
    # With start_server=False the adapter does not own the process, so cold_reopen
    # must be a no-op (the operator restarts an external server) and never touch a
    # missing process handle.
    from quiver_bench.competitors.quiver_adapter import QuiverAdapter

    adapter = QuiverAdapter(start_server=False)
    assert adapter._proc is None
    adapter.cold_reopen()  # must not raise
    assert adapter._proc is None
