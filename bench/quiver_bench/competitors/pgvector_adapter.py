# SPDX-License-Identifier: AGPL-3.0-only
"""pgvector adapter — PostgreSQL 16 + pgvector extension via Docker.

Requires Docker and ``psycopg2-binary``.  Uses an IVFFlat index with
``nprobe`` as the sweep parameter.
"""

from __future__ import annotations

import secrets
import subprocess
import time
import uuid

import numpy as np

from ..rss import docker_rss_mb
from .base import CompetitorAdapter

PGVECTOR_IMAGE = "pgvector/pgvector:pg16"
CONTAINER_NAME = f"quiver_bench_pgvector_{uuid.uuid4().hex[:8]}"
PG_PORT = 15432
PG_USER = "bench"
PG_DB = "bench"
TABLE = "vecs"


class PgvectorAdapter(CompetitorAdapter):
    name = "pgvector"
    version = "0.7/pg16"
    param_name = "nprobe"

    def __init__(self) -> None:
        self._container: str | None = None
        self._conn = None
        self._metric = "l2"
        self._dim = 0
        self._n = 0
        self._pg_pass = secrets.token_urlsafe(24)

    def start(self) -> None:
        subprocess.run(["docker", "pull", PGVECTOR_IMAGE], check=False, capture_output=True)
        self._container = CONTAINER_NAME
        subprocess.run(
            [
                "docker", "run", "-d",
                "--name", self._container,
                "-p", f"127.0.0.1:{PG_PORT}:5432",
                "-e", f"POSTGRES_USER={PG_USER}",
                "-e", f"POSTGRES_PASSWORD={self._pg_pass}",
                "-e", f"POSTGRES_DB={PG_DB}",
                PGVECTOR_IMAGE,
            ],
            check=True,
            capture_output=True,
        )
        # Wait for Postgres to be ready
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                self._connect()
                break
            except Exception:  # noqa: BLE001
                time.sleep(1)

    def _connect(self):
        import psycopg2  # type: ignore[import]

        conn = psycopg2.connect(
            host="127.0.0.1",
            port=PG_PORT,
            user=PG_USER,
            password=self._pg_pass,
            dbname=PG_DB,
            connect_timeout=3,
        )
        conn.autocommit = True
        return conn

    def stop(self) -> None:
        if self._conn is not None:
            try:
                self._conn.close()
            except Exception:  # noqa: BLE001
                pass
        if self._container is not None:
            subprocess.run(["docker", "stop", self._container], capture_output=True, check=False)
            subprocess.run(["docker", "rm", self._container], capture_output=True, check=False)
        self._conn = None
        self._container = None

    def build(self, base_vectors: np.ndarray, metric: str) -> tuple[float, float | None]:
        try:
            import psycopg2  # type: ignore[import]
            from psycopg2.extras import execute_values  # type: ignore[import]
        except ImportError as exc:
            raise RuntimeError("psycopg2-binary is required; run: uv pip install psycopg2-binary") from exc

        self._metric = metric
        n, dim = base_vectors.shape
        self._dim = dim
        self._n = n
        pg_op = "<->" if metric == "l2" else "<=>"

        conn = self._connect()
        self._conn = conn
        cur = conn.cursor()

        cur.execute("CREATE EXTENSION IF NOT EXISTS vector")
        cur.execute(f"DROP TABLE IF EXISTS {TABLE}")
        cur.execute(f"CREATE TABLE {TABLE} (id int, vec vector({dim}))")

        start = time.perf_counter()
        batch = 500
        for lo in range(0, n, batch):
            chunk = base_vectors[lo : lo + batch]
            rows = [(lo + j, vec.tolist()) for j, vec in enumerate(chunk)]
            execute_values(cur, f"INSERT INTO {TABLE} (id, vec) VALUES %s", rows)

        # Build IVFFlat index
        n_lists = min(int(n ** 0.5), 4096)
        idx_op = "vector_l2_ops" if metric == "l2" else "vector_cosine_ops"
        cur.execute(
            f"CREATE INDEX ON {TABLE} USING ivfflat (vec {idx_op}) WITH (lists = {n_lists})"
        )
        build_s = time.perf_counter() - start

        # Index size
        cur.execute(f"SELECT pg_total_relation_size('{TABLE}')")
        size_bytes = cur.fetchone()[0]
        disk_mb = size_bytes / (1024 * 1024)

        return build_s, disk_mb

    def query_one(self, query: np.ndarray, k: int, param: int) -> list[int]:
        cur = self._conn.cursor()
        op = "<->" if self._metric == "l2" else "<=>"
        cur.execute(f"SET ivfflat.probes = {param}")
        cur.execute(
            f"SELECT id FROM {TABLE} ORDER BY vec {op} %s::vector LIMIT {k}",
            (query.tolist(),),
        )
        return [row[0] for row in cur.fetchall()]

    def sample_rss(self) -> float | None:
        if self._container is None:
            return None
        return docker_rss_mb(self._container)
