# SPDX-License-Identifier: AGPL-3.0-only
"""RSS (resident set size) measurement helpers.

Supports two modes:
- Native process (self or a child PID): reads /proc/<pid>/status on Linux.
- Docker container: runs ``docker stats --no-stream`` and parses the MemUsage field.

All values are returned in **megabytes (float)**.  Returns None when measurement
is not possible (e.g. the pid/container has already exited).
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path


def native_rss_mb(pid: int | None = None) -> float | None:
    """Return RSS in MB for *pid* (defaults to the current process).

    Works on Linux via /proc; falls back to psutil when available.
    Returns None on failure.
    """
    target = pid if pid is not None else _self_pid()
    proc_path = Path(f"/proc/{target}/status")
    if proc_path.exists():
        try:
            text = proc_path.read_text()
            m = re.search(r"^VmRSS:\s+(\d+)\s+kB", text, re.MULTILINE)
            if m:
                return int(m.group(1)) / 1024.0
        except OSError:
            pass
    # psutil fallback
    try:
        import psutil  # type: ignore[import]

        p = psutil.Process(target)
        return p.memory_info().rss / (1024 * 1024)
    except Exception:  # noqa: BLE001
        return None


def docker_rss_mb(container: str) -> float | None:
    """Return RSS in MB for a running Docker *container* name/id.

    Parses the ``MemUsage`` column of ``docker stats --no-stream``.
    E.g. ``512MiB / 15.6GiB`` → 512.0.
    Returns None on failure or if Docker is not available.
    """
    try:
        out = subprocess.check_output(
            [
                "docker",
                "stats",
                "--no-stream",
                "--format",
                "{{.MemUsage}}",
                container,
            ],
            stderr=subprocess.DEVNULL,
            timeout=10,
        ).decode()
    except Exception:  # noqa: BLE001
        return None
    # Format: "512MiB / 15.6GiB"  or  "1.23GiB / 15.6GiB"
    usage = out.strip().split("/")[0].strip()
    return _parse_mem(usage)


def _parse_mem(s: str) -> float | None:
    """Parse a Docker memory string like '512MiB', '1.5GiB', '256kB' → MB."""
    m = re.match(r"([\d.]+)\s*([kKmMgGtT]i?[bB]?)", s.strip())
    if not m:
        return None
    val = float(m.group(1))
    unit = m.group(2).lower().rstrip("b")
    multipliers = {"k": 1 / 1024, "ki": 1 / 1024, "m": 1.0, "mi": 1.0, "g": 1024.0, "gi": 1024.0}
    factor = multipliers.get(unit)
    return val * factor if factor is not None else None


def _self_pid() -> int:
    import os

    return os.getpid()
