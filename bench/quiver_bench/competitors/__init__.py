# SPDX-License-Identifier: AGPL-3.0-only
"""Multi-DB competitor adapters for the Quiver benchmark suite.

Each adapter implements :class:`CompetitorAdapter` and manages its own
lifecycle (Docker containers, index build, RSS sampling, query sweep).
"""

from .base import BenchResult, CompetitorAdapter

__all__ = ["BenchResult", "CompetitorAdapter"]
