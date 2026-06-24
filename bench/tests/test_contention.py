# SPDX-License-Identifier: AGPL-3.0-only
"""Read-during-write contention driver (ADR-0064, measure-first)."""

from __future__ import annotations

import threading

from quiver_bench.contention_sweep import _drive


def test_drive_counts_reads_without_a_writer():
    counter = {"n": 0}
    lock = threading.Lock()

    def read_fn() -> None:
        with lock:
            counter["n"] += 1

    reads, writes, elapsed = _drive(read_fn, workers=4, duration_s=0.2)
    assert reads > 0
    assert writes == 0
    assert reads == counter["n"]  # every counted read was returned
    assert 0.15 < elapsed < 1.0  # ran ~the requested window


def test_drive_runs_reader_and_writer_concurrently():
    reads = {"n": 0}
    writes = {"n": 0}
    rlock = threading.Lock()
    wlock = threading.Lock()

    def read_fn() -> None:
        with rlock:
            reads["n"] += 1

    def write_fn() -> None:
        with wlock:
            writes["n"] += 1

    nreads, nwrites, _ = _drive(read_fn, workers=2, duration_s=0.2, write_fn=write_fn)
    assert nreads > 0 and nwrites > 0  # both ran
    assert nwrites == writes["n"]
