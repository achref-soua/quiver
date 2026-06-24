# SPDX-License-Identifier: AGPL-3.0-only
"""Quiver adapter — connects to a running Quiver server via the Python SDK.

The server must already be running; this adapter does NOT start/stop it.
Use ``scripts/acceptance.sh`` (alt ports 7333/7334) or ``quiver serve`` to
start a server before invoking the benchmark comparison.
"""

from __future__ import annotations

import subprocess
import tempfile
import time

import numpy as np

from .base import CompetitorAdapter

QUIVER_VERSION = "0.20.0"
COLLECTION = "quiver_bench"

# Keep each bulk request comfortably under the server's default 32 MiB body cap
# (QUIVER_MAX_REQUEST_BODY_BYTES) while still feeding the deferred single-rebuild
# path one big batch at a time. We size the batch from the vector dimension.
# The per-component estimate must cover the WORST case: real-valued datasets
# (e.g. GIST) serialize each float32 as a long Python repr like
# "0.0345098039215686," (~20 chars), unlike SIFT's small integer-valued floats
# ("12.0,"). 22 bytes/component + a 20 MiB target leaves headroom under 32 MiB.
_BULK_TARGET_BYTES = 20 * 1024 * 1024
_BYTES_PER_FLOAT_TEXT = 22
_MAX_BULK_BATCH = 50_000  # server default QUIVER_MAX_BULK_BATCH_SIZE


def bulk_batch_size(dim: int) -> int:
    """Largest bulk batch for ``dim``-d vectors that stays under the body cap.

    Bounded above by the server's ``max_bulk_batch_size`` and below by 1, so a
    single huge vector still makes progress.
    """
    by_bytes = _BULK_TARGET_BYTES // (dim * _BYTES_PER_FLOAT_TEXT)
    return max(1, min(_MAX_BULK_BATCH, by_bytes))


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
        index: str | None = None,
        pq_subspaces: int | None = None,
    ) -> None:
        self._url = url
        self._api_key = api_key
        self._start_server = start_server
        self._data_dir = data_dir
        # Index structure + quantization for the memory-wedge sweep. None keeps the
        # server default (in-memory HNSW, exact vectors); "ivf"/"disk_vamana" with
        # pq_subspaces trade recall for a smaller resident footprint.
        self._index = index
        self._pq_subspaces = pq_subspaces
        self._proc: subprocess.Popen | None = None
        self._tmpdir: tempfile.TemporaryDirectory | None = None
        self._data_path: str | None = None  # persistent across a cold reopen
        self._client = None
        self._metric = "l2"

    @property
    def config_label(self) -> str:
        """Short human label for this adapter's index/quantization config."""
        idx = self._index or "hnsw"
        return f"{idx}+pq{self._pq_subspaces}" if self._pq_subspaces else idx

    def start(self) -> None:
        if self._start_server:
            # `quiver serve` is configured by environment, not flags. A fresh
            # process + data dir per config is what makes the memory-wedge RSS
            # honest: each config's resident set is its own, not a shared
            # server's cumulative high-water mark. ponytail: live shell, exercised
            # by the isolated wedge run, not a unit test.
            self._tmpdir = tempfile.TemporaryDirectory()
            self._data_path = self._data_dir or self._tmpdir.name
            self._start_proc()
        self._connect()

    def _start_proc(self) -> None:
        """Spawn `quiver serve` on the persistent data dir and wait until ready.
        Reused by `start` and `cold_reopen` so a restart reopens the same data."""
        import os
        import urllib.request
        from urllib.parse import urlparse

        parsed = urlparse(self._url)
        rest_addr = f"{parsed.hostname or '127.0.0.1'}:{parsed.port or 6333}"
        grpc_addr = f"{parsed.hostname or '127.0.0.1'}:{(parsed.port or 6333) + 1}"
        env = {
            **os.environ,
            "QUIVER_INSECURE": "true",  # bench server, no real data
            "QUIVER_DATA_DIR": self._data_path,
            "QUIVER_REST_ADDR": rest_addr,
            "QUIVER_GRPC_ADDR": grpc_addr,
        }
        if self._api_key:
            env["QUIVER_API_KEYS"] = self._api_key
        self._proc = subprocess.Popen(
            ["quiver", "serve"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                urllib.request.urlopen(f"{self._url}/readyz", timeout=2)
                return
            except Exception:  # noqa: BLE001
                time.sleep(0.5)

    def _connect(self) -> None:
        from quiver import Client  # type: ignore[import]

        # Generous timeout: the bulk path defers the whole index build to the
        # first query (the forced rebuild in build()), which on 1M vectors takes
        # well over the SDK's default 30s — especially at 960-d (GIST1M).
        self._client = Client(self._url, api_key=self._api_key, timeout=3600.0)

    def cold_reopen(self) -> None:
        """Restart the server on its built data so RSS reflects the *serving*
        footprint, not the build's allocator high-water mark (ADR-0061/0063). The
        disk-resident index then loads its mmap base instead of rebuilding, so only
        the PQ codes stay resident — the memory wedge. No-op for an externally
        managed server (the operator restarts it; see the reference-hardware
        runbook)."""
        if not self._start_server or self._proc is None:
            return
        # Force a checkpoint so the durable index blob is sealed (ADR-0063): the
        # snapshot endpoint checkpoints, then copies to a throwaway dir we delete.
        # ponytail: reuse snapshot to trigger the checkpoint — the copy is wasted
        # I/O, fine for a one-time wedge prep; add a bare checkpoint endpoint if it
        # ever costs too much at 10M scale.
        import os
        import shutil
        import tempfile as _tf

        dest = os.path.join(_tf.gettempdir(), f"quiver_ckpt_{os.getpid()}_{time.time_ns()}")
        try:
            self._client._send("POST", "/v1/snapshot", {"destination": dest})
        finally:
            shutil.rmtree(dest, ignore_errors=True)
        # Cold reopen: stop the build-heavy process, start a fresh one on the same
        # data dir (durable load → frugal), reconnect, and let query_sweep's warmup
        # touch the working set before it samples RSS.
        self._proc.terminate()
        self._proc.wait(timeout=30)
        self._start_proc()
        self._connect()

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
        dim = int(base_vectors.shape[1])
        try:
            self._client.delete_collection(COLLECTION)
        except Exception:  # noqa: BLE001
            pass
        self._client.create_collection(
            COLLECTION,
            dim=dim,
            metric=metric,
            index=self._index,
            pq_subspaces=self._pq_subspaces,
        )

        # Bulk-ingest path (ADR-0045): each request hits POST …/points:bulk, which
        # does one WAL fsync and marks the index stale, deferring the index build.
        # We batch only to stay under the 32 MiB request-body cap, not per-batch
        # index passes. The deferred rebuild then happens lazily on the first
        # search — so to report an honest "time until queryable" build number we
        # force that rebuild with one query INSIDE the timer (competitors' build
        # numbers all include index construction).
        batch = bulk_batch_size(dim)
        start = time.perf_counter()
        for lo in range(0, base_vectors.shape[0], batch):
            chunk = base_vectors[lo : lo + batch]
            points = [{"id": str(lo + j), "vector": vec.tolist()} for j, vec in enumerate(chunk)]
            self._client._send("POST", f"/v1/collections/{COLLECTION}/points:bulk", {"points": points})
        # Force the deferred index build (stale → rebuilt on first read).
        self._client.search(COLLECTION, base_vectors[0].tolist(), k=10, ef_search=64, with_payload=False)
        build_s = time.perf_counter() - start
        return build_s, None

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        hits = self._client.search(COLLECTION, query.tolist(), k=k, ef_search=param, with_payload=False)
        return [int(h.id) for h in hits]

    def write_batch(self, point_ids: list[str], vectors: list[np.ndarray]) -> None:
        """Upsert a batch of points in one request — the write-load generator for the
        read-during-write contention sweep (ADR-0064). One upsert request is one WAL
        fsync (bulk batches one fsync, ADR-0038), and under the RwLock it holds the
        exclusive lock for that whole window, blocking concurrent reads; MVCC removes
        that. A larger batch is a longer exclusive-lock window (the write-*size*
        pressure dimension); re-upserting existing ids exercises the live incremental
        update path."""
        points = [{"id": pid, "vector": v.tolist()} for pid, v in zip(point_ids, vectors)]
        self._client.upsert(COLLECTION, points)

    def sample_rss(self) -> float | None:
        from urllib.parse import urlparse

        from ..rss import native_rss_mb, pid_listening_on

        # Measure the Quiver *server* process (found by the port it listens on),
        # not this Python client — the client holds the dataset and would report a
        # meaningless figure for Quiver's headline memory metric.
        port = urlparse(self._url).port or 6333
        pid = pid_listening_on(port)
        return native_rss_mb(pid) if pid is not None else None
