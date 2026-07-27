# SPDX-License-Identifier: AGPL-3.0-only
"""Hardware + software manifest for reproducibility.

Captured once at comparison-run start and written to manifest.json so
every result set carries its provenance.
"""

from __future__ import annotations

import json
import os
import platform
import subprocess
from datetime import datetime, timezone
from pathlib import Path


def capture() -> dict:
    """Return a dict describing the current hardware and software environment."""
    return {
        "timestamp": datetime.now(tz=timezone.utc).isoformat(),
        "os": platform.system(),
        "os_release": platform.release(),
        "os_version": platform.version(),
        "machine": platform.machine(),
        # `platform.processor()` returns the bare architecture ("x86_64") on Linux,
        # which is useless for reproducing a run — always prefer the model name.
        "cpu_model": _lscpu_model(),
        "processor": platform.processor() or _lscpu_model(),
        "python": platform.python_version(),
        "cpu_count_logical": os.cpu_count(),
        "cpu_count_physical": _physical_cores(),
        "ram_total_mb": _ram_total_mb(),
        "ram_available_mb": _ram_available_mb(),
        "swap_total_mb": _swap_total_mb(),
        "disk_free_gb": _disk_free_gb(),
        "load_average": os.getloadavg(),
        # Which build actually produced these numbers. Without this a result set
        # cannot say what it measured, and a stale binary on PATH is invisible.
        "quiver_version": _quiver_version(),
        "rust_version": _cmd(["rustc", "--version"]),
        "cargo_version": _cmd(["cargo", "--version"]),
        "docker_version": _cmd(["docker", "--version"]),
        "note": (
            "Reference hardware for this project is the laptop described by the "
            "fields above, running under WSL2. That is a legitimate, reproducible "
            "data point and it is labelled as one: it is not a datacentre result, "
            "and absolute numbers should not be read as one. Comparative standings "
            "are meaningful because every system is measured on this same box with "
            "the same data. `load_average` is recorded so a run taken on a busy "
            "machine can be recognised as such rather than quietly published. "
            "See docs/benchmarks/methodology.md."
        ),
    }


def write(directory: Path, data: dict) -> None:
    (directory / "manifest.json").write_text(json.dumps(data, indent=2))


def _cmd(args: list[str]) -> str:
    try:
        return subprocess.check_output(args, stderr=subprocess.DEVNULL, timeout=5).decode().strip()
    except Exception:  # noqa: BLE001
        return "unavailable"


def _ram_total_mb() -> float | None:
    mem_info = Path("/proc/meminfo")
    if mem_info.exists():
        for line in mem_info.read_text().splitlines():
            if line.startswith("MemTotal:"):
                parts = line.split()
                if len(parts) >= 2:
                    return int(parts[1]) / 1024.0
    try:
        import psutil  # type: ignore[import]

        return psutil.virtual_memory().total / (1024 * 1024)
    except Exception:  # noqa: BLE001
        return None


def _quiver_version() -> str:
    try:
        from .competitors.quiver_adapter import quiver_binary

        return f"{_cmd([quiver_binary(), '--version'])} ({quiver_binary()})"
    except Exception:  # noqa: BLE001
        return "unavailable"


def _physical_cores() -> int | None:
    """Physical cores, which bound real parallelism (logical counts hyperthreads)."""
    try:
        out = subprocess.check_output(["lscpu"], stderr=subprocess.DEVNULL, timeout=5).decode()
        sockets = cores_per_socket = None
        for line in out.splitlines():
            if line.startswith("Core(s) per socket:"):
                cores_per_socket = int(line.split(":", 1)[1].strip())
            elif line.startswith("Socket(s):"):
                sockets = int(line.split(":", 1)[1].strip())
        if sockets and cores_per_socket:
            return sockets * cores_per_socket
    except Exception:  # noqa: BLE001
        pass
    return None


def _meminfo_mb(key: str) -> float | None:
    mem_info = Path("/proc/meminfo")
    if not mem_info.exists():
        return None
    for line in mem_info.read_text().splitlines():
        if line.startswith(key):
            parts = line.split()
            if len(parts) >= 2:
                return int(parts[1]) / 1024.0
    return None


def _ram_available_mb() -> float | None:
    """Available (not merely free) RAM — what a build actually has to work with."""
    return _meminfo_mb("MemAvailable:")


def _swap_total_mb() -> float | None:
    """Swap matters: a run that completes only by swapping is not the same result."""
    return _meminfo_mb("SwapTotal:")


def _disk_free_gb() -> float | None:
    try:
        usage = os.statvfs(".")
        return usage.f_bavail * usage.f_frsize / (1024**3)
    except Exception:  # noqa: BLE001
        return None


def _lscpu_model() -> str:
    try:
        out = subprocess.check_output(["lscpu"], stderr=subprocess.DEVNULL, timeout=5).decode()
        for line in out.splitlines():
            if "Model name" in line:
                return line.split(":", 1)[1].strip()
    except Exception:  # noqa: BLE001
        pass
    return platform.processor() or "unknown"
