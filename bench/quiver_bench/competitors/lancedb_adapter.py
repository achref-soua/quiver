# SPDX-License-Identifier: AGPL-3.0-only
"""LanceDB adapter — local Lance disk-resident IVF_PQ index.

No Docker required.  Requires ``lancedb>=0.12`` and ``pyarrow``.
Runs in-process; LanceDB manages its own data files in a temp directory.
"""

from __future__ import annotations

import shutil
import tempfile
import time
from pathlib import Path

import numpy as np

from ..rss import native_rss_mb
from .base import CompetitorAdapter

LANCEDB_VERSION = "0.33.0"
TABLE_NAME = "bench"


class LanceDBAdapter(CompetitorAdapter):
    name = "lancedb"
    version = LANCEDB_VERSION
    param_name = "nprobes"

    def __init__(self, n_partitions: int = 256, n_sub_vectors: int = 16) -> None:
        self._n_partitions = n_partitions
        self._n_sub_vectors = n_sub_vectors
        self._tmpdir: tempfile.TemporaryDirectory | None = None
        self._db = None
        self._table = None
        self._metric = "l2"

    def start(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()

    def stop(self) -> None:
        self._table = None
        self._db = None
        if self._tmpdir is not None:
            self._tmpdir.cleanup()

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        try:
            import lancedb  # type: ignore[import]
            import pyarrow as pa  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("lancedb and pyarrow are required; run: uv pip install lancedb pyarrow") from exc

        self._metric = metric
        lance_metric = "cosine" if metric == "cosine" else "L2"

        self._db = lancedb.connect(self._tmpdir.name)  # type: ignore[union-attr]

        n, dim = base_vectors.shape
        ids = list(range(n))
        schema = pa.schema([pa.field("id", pa.int32()), pa.field("vector", pa.list_(pa.float32(), dim))])
        batch = pa.record_batch(
            {"id": pa.array(ids, pa.int32()), "vector": pa.array(base_vectors.tolist(), pa.list_(pa.float32(), dim))},
            schema=schema,
        )

        start = time.perf_counter()
        if TABLE_NAME in self._db.table_names():
            self._db.drop_table(TABLE_NAME)
        self._table = self._db.create_table(TABLE_NAME, data=[batch], schema=schema)
        n_parts = min(self._n_partitions, max(1, n // 100))
        n_sub = min(self._n_sub_vectors, dim)
        # IVF_PQ requires ≥ 256 rows; fall back to flat (no index) for small sets.
        if n >= 256:
            try:
                self._table.create_index(
                    metric=lance_metric,
                    num_partitions=n_parts,
                    num_sub_vectors=n_sub,
                )
            except Exception:  # noqa: BLE001
                pass  # flat search will be used instead
        build_s = time.perf_counter() - start

        # Measure index disk size
        data_path = Path(self._tmpdir.name) / f"{TABLE_NAME}.lance"  # type: ignore[union-attr]
        disk_mb: float | None = None
        if data_path.exists():
            total = sum(f.stat().st_size for f in data_path.rglob("*") if f.is_file())
            disk_mb = total / (1024 * 1024)

        return build_s, disk_mb

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        results = (
            self._table.search(query.tolist()).metric(  # type: ignore[union-attr]
                "cosine" if self._metric == "cosine" else "L2"
            ).nprobes(param).limit(k).to_arrow()
        )
        return [int(x) for x in results["id"].to_pylist()]

    def sample_rss(self) -> float | None:
        return native_rss_mb()
