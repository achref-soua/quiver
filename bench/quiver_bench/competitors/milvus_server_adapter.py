# SPDX-License-Identifier: AGPL-3.0-only
"""Milvus *server* adapter — runs standalone Milvus in Docker (not Milvus Lite).

Milvus Lite is an in-process prototyping build and is not performance-representative
of production Milvus; this adapter runs the real standalone server (embedded-etcd
single container, as in Milvus's official ``standalone_embed.sh``) so the number is
fair. Requires Docker and ``pymilvus>=2.4``.

The HNSW index is built and the collection is **loaded** (segments sealed via
``flush`` and the load state polled to ``Loaded``) before any query runs — an
under-indexed Milvus would fall back to brute force and report a misleadingly slow
number. The container image is pinned via ``MILVUS_SERVER_IMAGE`` (default below).
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

import numpy as np

from ..rss import docker_rss_mb
from .base import CompetitorAdapter

MILVUS_IMAGE = os.environ.get("MILVUS_SERVER_IMAGE", "milvusdb/milvus:v2.5.4")
CONTAINER_NAME = f"quiver_bench_milvus_{uuid.uuid4().hex[:8]}"
GRPC_PORT = 19530
HEALTH_PORT = 9091
COLLECTION = "bench"

# Verbatim from Milvus's official standalone_embed.sh — the embedded-etcd config
# that lets standalone run as a single container with local storage (no MinIO).
_EMBED_ETCD_YAML = """\
listen-client-urls: http://0.0.0.0:2379
advertise-client-urls: http://0.0.0.0:2379
quota-backend-bytes: 4294967296
auto-compaction-mode: revision
auto-compaction-retention: '1000'
"""


class MilvusServerAdapter(CompetitorAdapter):
    name = "milvus_server"
    version = MILVUS_IMAGE.split(":")[-1]
    param_name = "ef_search"

    def __init__(self) -> None:
        self._container: str | None = None
        self._client = None
        self._metric = "l2"
        self._tmpdir: tempfile.TemporaryDirectory | None = None

    def start(self) -> None:
        # Only the tiny config files are bind-mounted; Milvus writes its data to
        # the container's own (ephemeral) filesystem, which `docker rm` reclaims —
        # so there are no root-owned host files to clean up afterwards.
        self._tmpdir = tempfile.TemporaryDirectory(ignore_cleanup_errors=True)
        cfg = Path(self._tmpdir.name)
        (cfg / "embedEtcd.yaml").write_text(_EMBED_ETCD_YAML)
        (cfg / "user.yaml").write_text("")

        subprocess.run(["docker", "pull", MILVUS_IMAGE], check=False, capture_output=True)
        self._container = CONTAINER_NAME
        subprocess.run(
            [
                "docker", "run", "-d",
                "--name", self._container,
                "--security-opt", "seccomp=unconfined",
                "-e", "ETCD_USE_EMBED=true",
                "-e", "ETCD_DATA_DIR=/var/lib/milvus/etcd",
                "-e", "ETCD_CONFIG_PATH=/milvus/configs/embedEtcd.yaml",
                "-e", "COMMON_STORAGETYPE=local",
                "-v", f"{cfg / 'embedEtcd.yaml'}:/milvus/configs/embedEtcd.yaml",
                "-v", f"{cfg / 'user.yaml'}:/milvus/configs/user.yaml",
                "-p", f"127.0.0.1:{GRPC_PORT}:19530",
                "-p", f"127.0.0.1:{HEALTH_PORT}:9091",
                MILVUS_IMAGE,
                "milvus", "run", "standalone",
            ],
            check=True,
            capture_output=True,
        )

        # Milvus standalone takes 30–90 s to become healthy.
        import urllib.request

        deadline = time.time() + 240
        while time.time() < deadline:
            try:
                resp = urllib.request.urlopen(
                    f"http://127.0.0.1:{HEALTH_PORT}/healthz", timeout=3
                )
                if resp.status == 200:
                    break
            except Exception:  # noqa: BLE001
                time.sleep(2)
        else:
            raise RuntimeError("milvus server did not become healthy within 240s")

        try:
            from pymilvus import MilvusClient  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("pymilvus is required; run: uv pip install pymilvus") from exc
        # A brief grace period: the proxy accepts connections a moment after healthz.
        last_exc: Exception | None = None
        for _ in range(30):
            try:
                self._client = MilvusClient(uri=f"http://127.0.0.1:{GRPC_PORT}")
                self._client.list_collections()
                break
            except Exception as exc:  # noqa: BLE001
                last_exc = exc
                time.sleep(2)
        else:
            raise RuntimeError(f"could not connect to milvus server: {last_exc}")

    def stop(self) -> None:
        self._client = None
        if self._container is not None:
            subprocess.run(["docker", "stop", self._container], capture_output=True, check=False)
            subprocess.run(["docker", "rm", self._container], capture_output=True, check=False)
            self._container = None
        if self._tmpdir is not None:
            self._tmpdir.cleanup()
            self._tmpdir = None

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        self._metric = metric
        milvus_metric = "IP" if metric == "cosine" else "L2"
        n, dim = base_vectors.shape

        if self._client.has_collection(COLLECTION):  # type: ignore[union-attr]
            self._client.drop_collection(COLLECTION)  # type: ignore[union-attr]
        self._client.create_collection(  # type: ignore[union-attr]
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
            self._client.insert(collection_name=COLLECTION, data=data)  # type: ignore[union-attr]

        # Seal segments and wait until the collection is fully loaded (HNSW index
        # built) — otherwise queries would hit unindexed growing segments and the
        # latency would not reflect Milvus's real HNSW performance.
        self._client.flush(COLLECTION)  # type: ignore[union-attr]
        self._client.load_collection(COLLECTION)  # type: ignore[union-attr]
        deadline = time.time() + 300
        while time.time() < deadline:
            state = self._client.get_load_state(COLLECTION)  # type: ignore[union-attr]
            value = state.get("state") if isinstance(state, dict) else state
            if str(value).endswith("Loaded") or str(value) == "3":
                break
            time.sleep(1)
        build_s = time.perf_counter() - start
        return build_s, None

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
        if self._container is None:
            return None
        return docker_rss_mb(self._container)
