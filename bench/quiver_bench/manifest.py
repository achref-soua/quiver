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
        "processor": platform.processor() or _lscpu_model(),
        "python": platform.python_version(),
        "cpu_count_logical": os.cpu_count(),
        "ram_total_mb": _ram_total_mb(),
        "rust_version": _cmd(["rustc", "--version"]),
        "cargo_version": _cmd(["cargo", "--version"]),
        "docker_version": _cmd(["docker", "--version"]),
        "note": (
            "This benchmark ran on a WSL2 dev box (resource-shared). "
            "QPS and RSS numbers are labelled accordingly. "
            "See docs/benchmarks/reference-hardware-runbook.md for the "
            "procedure to produce official headline numbers on dedicated hardware."
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


def _lscpu_model() -> str:
    try:
        out = subprocess.check_output(["lscpu"], stderr=subprocess.DEVNULL, timeout=5).decode()
        for line in out.splitlines():
            if "Model name" in line:
                return line.split(":", 1)[1].strip()
    except Exception:  # noqa: BLE001
        pass
    return platform.processor() or "unknown"
