# SPDX-License-Identifier: AGPL-3.0-only
"""Pure metric helpers: recall@k and latency percentiles.

Kept free of numpy and any I/O so they are trivially unit-tested and define the
exact figures the harness reports (``docs/benchmarks/methodology.md``).
"""

from __future__ import annotations

from typing import Sequence


def recall_at_k(retrieved: Sequence[int], truth: Sequence[int], k: int) -> float:
    """Fraction of the top-``k`` exact neighbours that were retrieved.

    Compares the first ``k`` retrieved ids against the first ``k`` ground-truth
    ids (the standard ann-benchmarks recall@k). An empty truth set scores 1.0.
    """
    wanted = set(truth[:k])
    if not wanted:
        return 1.0
    hits = sum(1 for r in retrieved[:k] if r in wanted)
    return hits / len(wanted)


def mean_recall_at_k(
    retrieved: Sequence[Sequence[int]],
    truth: Sequence[Sequence[int]],
    k: int,
) -> float:
    """Mean recall@k over a query set. Returns 0.0 for an empty set."""
    if not retrieved:
        return 0.0
    total = sum(recall_at_k(r, t, k) for r, t in zip(retrieved, truth))
    return total / len(retrieved)


def percentile(values_ms: Sequence[float], p: float) -> float:
    """The ``p``-th percentile (0–100) by nearest-rank, in input units.

    Returns 0.0 for an empty input. ``percentile(xs, 50)`` is the median.
    """
    if not values_ms:
        return 0.0
    ordered = sorted(values_ms)
    rank = round((p / 100.0) * (len(ordered) - 1))
    index = max(0, min(len(ordered) - 1, int(rank)))
    return ordered[index]
