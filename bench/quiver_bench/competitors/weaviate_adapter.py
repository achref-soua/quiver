# SPDX-License-Identifier: AGPL-3.0-only
"""Weaviate adapter — Weaviate 1.24.6 in Docker with HNSW index.

Requires Docker and ``weaviate-client>=4.5``.
"""

from __future__ import annotations

import subprocess
import time
import uuid

import numpy as np

from ..rss import docker_rss_mb
from .base import CompetitorAdapter

WEAVIATE_IMAGE = "cr.weaviate.io/semitechnologies/weaviate:1.27.0"
CONTAINER_NAME = f"quiver_bench_weaviate_{uuid.uuid4().hex[:8]}"
HTTP_PORT = 18080
GRPC_PORT = 50051
CLASS_NAME = "Bench"


class WeaviateAdapter(CompetitorAdapter):
    name = "weaviate"
    version = "1.27.0"
    param_name = "ef_search"

    def __init__(self) -> None:
        self._container: str | None = None
        self._client = None
        self._metric = "l2"

    def start(self) -> None:
        subprocess.run(["docker", "pull", WEAVIATE_IMAGE], check=False, capture_output=True)
        self._container = CONTAINER_NAME
        subprocess.run(
            [
                "docker", "run", "-d",
                "--name", self._container,
                "-p", f"{HTTP_PORT}:8080",
                "-p", f"{GRPC_PORT}:{GRPC_PORT}",
                "-e", "AUTHENTICATION_ANONYMOUS_ACCESS_ENABLED=true",
                "-e", "PERSISTENCE_DATA_PATH=/var/lib/weaviate",
                "-e", "DEFAULT_VECTORIZER_MODULE=none",
                "-e", "ENABLE_MODULES=",
                WEAVIATE_IMAGE,
            ],
            check=True,
            capture_output=True,
        )
        import urllib.request

        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{HTTP_PORT}/v1/.well-known/ready", timeout=2)
                break
            except Exception:  # noqa: BLE001
                time.sleep(1)

        try:
            import weaviate  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("weaviate-client is required; run: uv pip install weaviate-client") from exc

        import weaviate  # type: ignore[import]
        import weaviate.classes as wvc  # type: ignore[import]

        self._client = weaviate.connect_to_local(
            host="127.0.0.1",
            port=HTTP_PORT,
            grpc_port=GRPC_PORT,
        )

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
        import weaviate.classes as wvc  # type: ignore[import]

        self._metric = metric
        n, dim = base_vectors.shape
        distance = wvc.config.VectorDistances.COSINE if metric == "cosine" else wvc.config.VectorDistances.L2_SQUARED

        if self._client.collections.exists(CLASS_NAME):  # type: ignore[union-attr]
            self._client.collections.delete(CLASS_NAME)  # type: ignore[union-attr]

        self._client.collections.create(  # type: ignore[union-attr]
            CLASS_NAME,
            vectorizer_config=wvc.config.Configure.Vectorizer.none(),
            vector_index_config=wvc.config.Configure.VectorIndex.hnsw(
                distance_metric=distance,
                ef_construction=200,
                max_connections=16,
            ),
        )
        collection = self._client.collections.get(CLASS_NAME)  # type: ignore[union-attr]

        start = time.perf_counter()
        batch = 500
        for lo in range(0, n, batch):
            chunk = base_vectors[lo : lo + batch]
            with collection.batch.dynamic() as b:
                for j, vec in enumerate(chunk):
                    b.add_object(properties={"orig_id": lo + j}, vector=vec.tolist())
        build_s = time.perf_counter() - start

        return build_s, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        import weaviate.classes as wvc  # type: ignore[import]

        collection = self._client.collections.get(CLASS_NAME)  # type: ignore[union-attr]
        results = collection.query.near_vector(
            near_vector=query.tolist(),
            limit=k,
            return_properties=["orig_id"],
            return_metadata=wvc.query.MetadataQuery(distance=False),
        )
        return [int(o.properties["orig_id"]) for o in results.objects]

    def sample_rss(self) -> float | None:
        if self._container is None:
            return None
        return docker_rss_mb(self._container)
