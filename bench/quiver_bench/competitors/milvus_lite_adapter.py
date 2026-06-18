# SPDX-License-Identifier: AGPL-3.0-only
"""Milvus Lite adapter — runs the Milvus engine in-process via milvus-lite.

No Docker required.  Requires ``milvus-lite>=2.4`` and ``pymilvus>=2.4``.
Milvus Lite stores its data in a temp file.
"""

from __future__ import annotations

import tempfile
import time
from pathlib import Path

import numpy as np

from ..rss import native_rss_mb
from .base import CompetitorAdapter

MILVUS_LITE_VERSION = "3.0.0"
COLLECTION = "bench"


class MilvusLiteAdapter(CompetitorAdapter):
    name = "milvus_lite"
    version = MILVUS_LITE_VERSION
    param_name = "ef_search"

    def __init__(self) -> None:
        self._tmpdir: tempfile.TemporaryDirectory | None = None
        self._client = None
        self._metric = "l2"

    def start(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()

    def stop(self) -> None:
        self._client = None
        if self._tmpdir is not None:
            self._tmpdir.cleanup()
            self._tmpdir = None

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        try:
            from pymilvus import MilvusClient  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError(
                "milvus-lite and pymilvus are required; run: uv pip install milvus-lite pymilvus"
            ) from exc

        self._metric = metric
        db_path = Path(self._tmpdir.name) / "milvus.db"  # type: ignore[union-attr]
        milvus_metric = "IP" if metric == "cosine" else "L2"

        self._client = MilvusClient(str(db_path))
        n, dim = base_vectors.shape

        if self._client.has_collection(COLLECTION):
            self._client.drop_collection(COLLECTION)

        self._client.create_collection(
            collection_name=COLLECTION,
            dimension=dim,
            metric_type=milvus_metric,
            index_type="HNSW",
            params={"M": 16, "efConstruction": 200},
        )

        start = time.perf_counter()
        batch = 500
        for lo in range(0, n, batch):
            chunk = base_vectors[lo : lo + batch]
            if metric == "cosine":
                norms = np.linalg.norm(chunk, axis=1, keepdims=True)
                norms[norms == 0] = 1.0
                chunk = chunk / norms
            data = [{"id": lo + j, "vector": vec.tolist()} for j, vec in enumerate(chunk)]
            self._client.insert(collection_name=COLLECTION, data=data)
        build_s = time.perf_counter() - start

        disk_mb: float | None = None
        if db_path.exists():
            disk_mb = db_path.stat().st_size / (1024 * 1024)

        return build_s, disk_mb

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        q = query.copy()
        if self._metric == "cosine":
            norm = np.linalg.norm(q)
            if norm > 0:
                q = q / norm
        results = self._client.search(  # type: ignore[union-attr]
            collection_name=COLLECTION,
            data=[q.tolist()],
            limit=k,
            search_params={"ef": param},
            output_fields=["id"],
        )
        return [int(r["id"]) for r in results[0]]

    def sample_rss(self) -> float | None:
        return native_rss_mb()
