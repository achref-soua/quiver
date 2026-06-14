# SPDX-License-Identifier: AGPL-3.0-only
"""Quiver — Python client for the security-first vector database.

Example::

    from quiver import Client, Point

    with Client(api_key="…") as q:
        q.create_collection("items", dim=3, metric="cosine")
        q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
        hits = q.search("items", [0.1, 0.2, 0.3], k=5)
"""

from .client import Client, CollectionInfo, FilterableField, Match, Point, QuiverError

__all__ = [
    "Client",
    "Point",
    "Match",
    "CollectionInfo",
    "FilterableField",
    "QuiverError",
]
__version__ = "0.1.0"
