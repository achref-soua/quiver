# SPDX-License-Identifier: AGPL-3.0-only
"""Qdrant adapter — runs Qdrant in Docker and queries via qdrant-client.

Requires Docker and ``qdrant-client>=1.9``.
Qdrant is pulled at the pinned version specified in ADR-0037.
"""

from __future__ import annotations

import subprocess
import threading
import time
import uuid

import numpy as np

from ..rss import docker_rss_mb
from .base import CompetitorAdapter

QDRANT_IMAGE = "qdrant/qdrant:v1.13.4"
CONTAINER_NAME = f"quiver_bench_qdrant_{uuid.uuid4().hex[:8]}"
REST_PORT = 16333
GRPC_PORT = 16334
COLLECTION = "bench"


class QdrantAdapter(CompetitorAdapter):
    name = "qdrant"
    version = "v1.13.4"
    param_name = "ef_search"

    def __init__(self) -> None:
        self._container: str | None = None
        self._client = None
        self._metric = "l2"
        # qdrant-client wraps a single httpx session that is not safe to share
        # across the saturated-QPS thread pool, so each worker thread gets its
        # own client. ponytail: per-thread clients are GC-closed when the pool's
        # threads exit; we don't track them for an explicit close.
        self._local = threading.local()

    def _thread_client(self):
        """A QdrantClient owned by the calling thread (created on first use)."""
        client = getattr(self._local, "client", None)
        if client is None:
            from qdrant_client import QdrantClient  # type: ignore[import]

            client = QdrantClient(url=f"http://127.0.0.1:{REST_PORT}")
            self._local.client = client
        return client

    def start(self) -> None:
        # Pull the image (no-op if already cached)
        subprocess.run(["docker", "pull", QDRANT_IMAGE], check=False, capture_output=True)
        self._container = CONTAINER_NAME
        subprocess.run(
            [
                "docker", "run", "-d",
                "--name", self._container,
                "-p", f"127.0.0.1:{REST_PORT}:6333",
                "-p", f"127.0.0.1:{GRPC_PORT}:6334",
                QDRANT_IMAGE,
            ],
            check=True,
            capture_output=True,
        )
        # Wait for readiness
        import urllib.request

        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{REST_PORT}/healthz", timeout=2)
                break
            except Exception:  # noqa: BLE001
                time.sleep(1)

        try:
            from qdrant_client import QdrantClient  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("qdrant-client is not installed; run: uv pip install qdrant-client") from exc
        self._client = QdrantClient(url=f"http://127.0.0.1:{REST_PORT}")

    def stop(self) -> None:
        if self._client is not None:
            try:
                self._client.close()
            except Exception:  # noqa: BLE001
                pass
        if self._container is not None:
            subprocess.run(["docker", "stop", self._container], capture_output=True, check=False)
            subprocess.run(["docker", "rm", self._container], capture_output=True, check=False)
        self._client = None
        self._container = None

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        from qdrant_client.models import Distance, VectorParams  # type: ignore[import]

        self._metric = metric
        n, dim = base_vectors.shape
        qdrant_dist = Distance.COSINE if metric == "cosine" else Distance.EUCLID

        try:
            self._client.delete_collection(COLLECTION)  # type: ignore[union-attr]
        except Exception:  # noqa: BLE001
            pass
        self._client.create_collection(  # type: ignore[union-attr]
            collection_name=COLLECTION,
            vectors_config=VectorParams(size=dim, distance=qdrant_dist),
        )

        from qdrant_client.models import PointStruct  # type: ignore[import]

        start = time.perf_counter()
        batch = 500
        for lo in range(0, n, batch):
            chunk = base_vectors[lo : lo + batch]
            points = [
                PointStruct(id=lo + j, vector=vec.tolist())
                for j, vec in enumerate(chunk)
            ]
            self._client.upsert(collection_name=COLLECTION, points=points)  # type: ignore[union-attr]
        build_s = time.perf_counter() - start
        return build_s, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        from qdrant_client.models import SearchParams  # type: ignore[import]

        # qdrant-client ≥ 1.10 removed `search`; use `query_points` instead.
        # Per-thread client so the saturated-QPS pass is genuinely concurrent.
        results = self._thread_client().query_points(
            collection_name=COLLECTION,
            query=query.tolist(),
            limit=k,
            search_params=SearchParams(hnsw_ef=param, exact=False),
            with_payload=False,
        )
        return [int(r.id) for r in results.points]

    def sample_rss(self) -> float | None:
        if self._container is None:
            return None
        return docker_rss_mb(self._container)
