# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the pure metric helpers and exact ground-truth generation."""

import numpy as np

from quiver_bench import mean_recall_at_k, percentile, recall_at_k
from quiver_bench.datasets import brute_force_l2, synthetic


def test_recall_at_k_counts_overlap_in_top_k():
    assert recall_at_k([1, 2, 3], [1, 2, 3], 3) == 1.0
    assert recall_at_k([1, 9, 3], [1, 2, 3], 3) == 2 / 3
    assert recall_at_k([9, 8, 7], [1, 2, 3], 3) == 0.0
    # Items beyond k do not count.
    assert recall_at_k([9, 9, 9, 1, 2, 3], [1, 2, 3], 3) == 0.0
    # Empty truth scores 1.0.
    assert recall_at_k([1], [], 3) == 1.0


def test_mean_recall_averages_over_queries():
    retrieved = [[1, 2], [3, 9]]
    truth = [[1, 2], [3, 4]]
    assert mean_recall_at_k(retrieved, truth, 2) == 0.75
    assert mean_recall_at_k([], [], 10) == 0.0


def test_percentile_nearest_rank():
    xs = [10.0, 20.0, 30.0, 40.0, 50.0]
    assert percentile(xs, 50) == 30.0
    assert percentile(xs, 0) == 10.0
    assert percentile(xs, 100) == 50.0
    assert percentile([], 95) == 0.0


def test_brute_force_l2_matches_known_nearest():
    base = np.array([[0.0, 0.0], [1.0, 0.0], [5.0, 5.0]], dtype=np.float32)
    queries = np.array([[0.1, 0.0]], dtype=np.float32)
    gt = brute_force_l2(base, queries, k=2)
    assert gt.shape == (1, 2)
    assert gt[0].tolist() == [0, 1]  # origin, then (1,0)


def test_synthetic_is_deterministic_and_self_consistent():
    a = synthetic(n=50, dim=8, queries=5, k=3, seed=7)
    b = synthetic(n=50, dim=8, queries=5, k=3, seed=7)
    assert np.array_equal(a.base, b.base)
    assert np.array_equal(a.ground_truth, b.ground_truth)
    assert a.ground_truth.shape == (5, 3)
    # The harness scores a perfect recall against its own exact ground truth.
    retrieved = [row.tolist() for row in a.ground_truth]
    truth = [row.tolist() for row in a.ground_truth]
    assert mean_recall_at_k(retrieved, truth, 3) == 1.0
