# SPDX-License-Identifier: AGPL-3.0-only
"""Quiver adapter — connects to a running Quiver server via the Python SDK.

The server must already be running; this adapter does NOT start/stop it.
Use ``scripts/acceptance.sh`` (alt ports 7333/7334) or ``quiver serve`` to
start a server before invoking the benchmark comparison.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
import time
from pathlib import Path

import numpy as np

from .base import BenchResult, CompetitorAdapter

QUIVER_VERSION = "0.17.0-dev"
COLLECTION = "quiver_bench"


class QuiverAdapter(CompetitorAdapter):
    name = "quiver"
    version = QUIVER_VERSION
    param_name = "ef_search"

    def __init__(
        self,
        url: str = "http://127.0.0.1:6333",
        api_key: str | None = None,
        *,
        start_server: bool = False,
        data_dir: str | None = None,
    ) -> None:
        self._url = url
        self._api_key = api_key
        self._start_server = start_server
        self._data_dir = data_dir
        self._proc: subprocess.Popen | None = None
        self._tmpdir: tempfile.TemporaryDirectory | None = None
        self._client = None
        self._metric = "l2"

    def start(self) -> None:
        if self._start_server:
            self._tmpdir = tempfile.TemporaryDirectory()
            data = self._data_dir or self._tmpdir.name
            key = self._api_key or "bench-key"
            self._proc = subprocess.Popen(
                ["quiver", "serve", "--data-dir", data, "--api-key", key, "--insecure"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            # Wait for the server to be ready
            import urllib.request

            deadline = time.time() + 30
            while time.time() < deadline:
                try:
                    urllib.request.urlopen(f"{self._url}/healthz", timeout=2)
                    break
                except Exception:  # noqa: BLE001
                    time.sleep(0.5)

        from quiver import Client  # type: ignore[import]

        self._client = Client(self._url, api_key=self._api_key)

    def stop(self) -> None:
        if self._client is not None:
            try:
                self._client.close()
            except Exception:  # noqa: BLE001
                pass
        if self._proc is not None:
            self._proc.terminate()
            self._proc.wait(timeout=5)
        if self._tmpdir is not None:
            self._tmpdir.cleanup()

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        self._metric = metric
        try:
            self._client.delete_collection(COLLECTION)
        except Exception:  # noqa: BLE001
            pass
        self._client.create_collection(COLLECTION, dim=int(base_vectors.shape[1]), metric=metric)
        start = time.perf_counter()
        batch = 500
        for lo in range(0, base_vectors.shape[0], batch):
            chunk = base_vectors[lo : lo + batch]
            points = [{"id": str(lo + j), "vector": vec.tolist()} for j, vec in enumerate(chunk)]
            self._client.upsert(COLLECTION, points)
        build_s = time.perf_counter() - start
        return build_s, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        hits = self._client.search(COLLECTION, query.tolist(), k=k, ef_search=param, with_payload=False)
        return [int(h.id) for h in hits]

    def sample_rss(self) -> float | None:
        from urllib.parse import urlparse

        from ..rss import native_rss_mb, pid_listening_on

        # Measure the Quiver *server* process (found by the port it listens on),
        # not this Python client — the client holds the dataset and would report a
        # meaningless figure for Quiver's headline memory metric.
        port = urlparse(self._url).port or 6333
        pid = pid_listening_on(port)
        return native_rss_mb(pid) if pid is not None else None
