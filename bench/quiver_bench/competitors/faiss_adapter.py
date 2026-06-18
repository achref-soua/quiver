# SPDX-License-Identifier: AGPL-3.0-only
"""FAISS adapter — uses faiss-cpu as the canonical library baseline.

No Docker required; runs in-process.  Requires ``faiss-cpu`` to be installed
(listed in bench/pyproject.toml as an optional competitor dependency).

We use ``IndexHNSWFlat`` (M=16) for L2 to mirror the Quiver HNSW default, with
the ef parameter swept.  For cosine we normalise vectors and use Inner Product.
"""

from __future__ import annotations

import time

import numpy as np

from ..rss import native_rss_mb
from .base import CompetitorAdapter

FAISS_VERSION = "1.14.3"


class FaissAdapter(CompetitorAdapter):
    name = "faiss"
    version = FAISS_VERSION
    param_name = "ef_search"

    def __init__(self, m: int = 16) -> None:
        self._m = m
        self._index = None
        self._metric = "l2"

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        try:
            import faiss  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("faiss-cpu is not installed; run: uv pip install faiss-cpu") from exc

        self._metric = metric
        n, dim = base_vectors.shape
        vecs = base_vectors.copy().astype(np.float32)

        if metric == "cosine":
            norms = np.linalg.norm(vecs, axis=1, keepdims=True)
            norms[norms == 0] = 1.0
            vecs /= norms
            self._index = faiss.IndexHNSWFlat(dim, self._m, faiss.METRIC_INNER_PRODUCT)
        else:
            self._index = faiss.IndexHNSWFlat(dim, self._m)

        self._index.hnsw.efConstruction = 200
        start = time.perf_counter()
        self._index.add(vecs)
        build_s = time.perf_counter() - start
        return build_s, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        self._index.hnsw.efSearch = param
        q = query.copy().astype(np.float32).reshape(1, -1)
        if self._metric == "cosine":
            norm = np.linalg.norm(q)
            if norm > 0:
                q /= norm
        _dists, ids = self._index.search(q, k)
        return [int(i) for i in ids[0] if i >= 0]

    def sample_rss(self) -> float | None:
        return native_rss_mb()
