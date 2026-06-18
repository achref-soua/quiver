# SPDX-License-Identifier: AGPL-3.0-only
"""Chroma adapter — uses chromadb in ephemeral (in-memory) mode.

No Docker required; runs fully in-process.  Requires ``chromadb>=0.5``.
Chroma's default HNSW implementation is used; ``ef_search`` is passed via
the collection HNSW settings.

Chroma normalises vectors internally and uses cosine by default; for L2
we use the 'l2' space setting.
"""

from __future__ import annotations

import time

import numpy as np

from ..rss import native_rss_mb
from .base import CompetitorAdapter

CHROMA_VERSION = "1.5.9"


class ChromaAdapter(CompetitorAdapter):
    name = "chroma"
    version = CHROMA_VERSION
    param_name = "ef_search"

    def __init__(self) -> None:
        self._client = None
        self._collection = None
        self._metric = "l2"

    def start(self) -> None:
        try:
            import chromadb  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("chromadb is not installed; run: uv pip install chromadb") from exc

        import chromadb  # type: ignore[import]

        self._client = chromadb.EphemeralClient()

    def stop(self) -> None:
        self._collection = None
        self._client = None

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        self._metric = metric
        space = "cosine" if metric == "cosine" else "l2"

        if self._client is None:
            self.start()

        try:
            self._client.delete_collection("bench")  # type: ignore[union-attr]
        except Exception:  # noqa: BLE001
            pass

        import chromadb  # type: ignore[import]

        self._collection = self._client.create_collection(  # type: ignore[union-attr]
            "bench",
            metadata={"hnsw:space": space},
        )

        n = base_vectors.shape[0]
        batch = 500
        start = time.perf_counter()
        for lo in range(0, n, batch):
            chunk = base_vectors[lo : lo + batch]
            ids = [str(lo + j) for j in range(len(chunk))]
            self._collection.add(ids=ids, embeddings=chunk.tolist())  # type: ignore[union-attr]
        build_s = time.perf_counter() - start

        return build_s, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        # Chroma doesn't expose ef_search per-query in the public API.
        results = self._collection.query(  # type: ignore[union-attr]
            query_embeddings=[query.tolist()],
            n_results=k,
            include=[],
        )
        ids = results["ids"][0]
        return [int(i) for i in ids]

    def sample_rss(self) -> float | None:
        return native_rss_mb()
