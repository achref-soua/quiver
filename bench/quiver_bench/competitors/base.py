# SPDX-License-Identifier: AGPL-3.0-only
"""Abstract base class for a benchmark competitor adapter.

Each subclass represents one ANN system (Quiver, Qdrant, LanceDB, …).
The comparison runner calls the lifecycle methods in order:
  1. ``start()``       — launch container / import libs
  2. ``build(ds)``     — upsert base vectors, record build_s + disk_mb
  3. ``warmup(ds, k)`` — 10 discarded queries (sets up any caches)
  4. ``sample_rss()``  — RSS after warmup, before timed run
  5. ``query(ds, k, param)`` (repeated per sweep point)
  6. ``stop()``        — tear down container / free resources
"""

from __future__ import annotations

import time
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Sequence


@dataclass
class BenchResult:
    """Measured outcome for one (competitor, dataset, sweep-param) triple."""

    competitor: str
    dataset: str
    param_name: str  # e.g. "ef_search" or "nprobe"
    param_value: int

    # Build phase
    build_s: float = 0.0
    index_disk_mb: float | None = None  # None = unmeasured

    # Memory
    rss_mb: float | None = None

    # Query quality + throughput (mean over reps)
    recall_at_10: float = 0.0
    qps_1t: float = 0.0
    p50_ms: float = 0.0
    p95_ms: float = 0.0
    p99_ms: float = 0.0

    # Variance (stdev over reps)
    recall_stdev: float = 0.0
    qps_stdev: float = 0.0

    # Metadata
    config: dict = field(default_factory=dict)
    notes: str = ""
    error: str = ""

    def as_dict(self) -> dict:
        import dataclasses

        return dataclasses.asdict(self)


class CompetitorAdapter(ABC):
    """One ANN system that can be benchmarked against Quiver."""

    #: Short identifier used in filenames and the report.
    name: str = "unknown"
    #: Human-readable version string (e.g. "qdrant v1.11.3").
    version: str = "unknown"
    #: The sweep parameter this system exposes (e.g. ef_search, nprobe).
    param_name: str = "param"

    def start(self) -> None:
        """Start the system (pull/run Docker container, import libs, etc.)."""

    def stop(self) -> None:
        """Tear down the system (stop container, free handles)."""

    @abstractmethod
    def build(self, base_vectors: "import numpy; numpy.ndarray", metric: str) -> tuple[float, float | None]:
        """Insert *base_vectors* and build the index.

        Returns ``(build_seconds, index_disk_mb_or_None)``.
        ``metric`` is 'l2' or 'cosine'.
        """

    def warmup(self, queries: "import numpy; numpy.ndarray", k: int, param: int) -> None:
        """Run 10 warm-up queries (results discarded)."""
        for q in queries[:10]:
            try:
                self.query_one(q, k, param)
            except Exception:  # noqa: BLE001
                pass

    @abstractmethod
    def query_one(self, query: "import numpy; numpy.ndarray", k: int, param: int) -> list[int]:
        """Run a single query and return a list of (up to k) retrieved IDs."""

    def sample_rss(self) -> float | None:
        """Sample the system's current RSS in MB. Override in subclasses."""
        return None

    def query_sweep(
        self,
        queries: "import numpy; numpy.ndarray",
        ground_truth: "import numpy; numpy.ndarray",
        k: int,
        params: Sequence[int],
        reps: int = 3,
    ) -> list[BenchResult]:
        """Run the full ef/nprobe sweep and return one BenchResult per param."""
        from ..metrics import mean_recall_at_k, percentile

        results = []
        for param in params:
            self.warmup(queries, k, param)
            rss = self.sample_rss()

            rep_recalls: list[float] = []
            rep_qps: list[float] = []
            all_latencies: list[float] = []

            for _ in range(reps):
                retrieved: list[list[int]] = []
                latencies_ms: list[float] = []
                for q in queries:
                    t0 = time.perf_counter()
                    ids = self.query_one(q, k, param)
                    latencies_ms.append((time.perf_counter() - t0) * 1000.0)
                    retrieved.append(ids)
                total_s = sum(latencies_ms) / 1000.0
                truth = [row.tolist() for row in ground_truth]
                rep_recalls.append(mean_recall_at_k(retrieved, truth, k))
                rep_qps.append(len(latencies_ms) / total_s if total_s > 0 else 0.0)
                all_latencies.extend(latencies_ms)

            import statistics

            recall_mean = statistics.mean(rep_recalls)
            qps_mean = statistics.mean(rep_qps)
            recall_stdev = statistics.stdev(rep_recalls) if len(rep_recalls) > 1 else 0.0
            qps_stdev = statistics.stdev(rep_qps) if len(rep_qps) > 1 else 0.0

            results.append(
                BenchResult(
                    competitor=self.name,
                    dataset="",
                    param_name=self.param_name,
                    param_value=param,
                    rss_mb=rss,
                    recall_at_10=round(recall_mean, 4),
                    qps_1t=round(qps_mean, 1),
                    p50_ms=round(percentile(all_latencies, 50), 3),
                    p95_ms=round(percentile(all_latencies, 95), 3),
                    p99_ms=round(percentile(all_latencies, 99), 3),
                    recall_stdev=round(recall_stdev, 4),
                    qps_stdev=round(qps_stdev, 1),
                    config={"metric": "l2"},
                )
            )
        return results
