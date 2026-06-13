# SPDX-License-Identifier: AGPL-3.0-only
"""Seed a small, recognizable demo collection for `just demo`.

Connects with the Python SDK using QUIVER_URL / QUIVER_API_KEY and upserts a
handful of toy "documents" so the cockpit and a first query have something to
show. The collection is encrypted at rest by the server it talks to.
"""

import os

from quiver import Client, Point

URL = os.environ.get("QUIVER_URL", "http://127.0.0.1:6333")
API_KEY = os.environ.get("QUIVER_API_KEY")

# Toy 4-d "embeddings" — three loose clusters (search / databases / ops).
DEMO = [
    Point("doc-1", [0.9, 0.1, 0.0, 0.1], {"title": "Vector search basics", "topic": "search"}),
    Point("doc-2", [0.8, 0.2, 0.1, 0.0], {"title": "HNSW explained", "topic": "search"}),
    Point("doc-3", [0.1, 0.9, 0.1, 0.0], {"title": "Storage engines", "topic": "databases"}),
    Point("doc-4", [0.0, 0.8, 0.2, 0.1], {"title": "Write-ahead logs", "topic": "databases"}),
    Point("doc-5", [0.1, 0.1, 0.9, 0.0], {"title": "Running in production", "topic": "ops"}),
    Point("doc-6", [0.0, 0.1, 0.8, 0.2], {"title": "Encryption at rest", "topic": "ops"}),
    Point("doc-7", [0.5, 0.5, 0.0, 0.1], {"title": "Indexing tradeoffs", "topic": "search"}),
    Point("doc-8", [0.2, 0.2, 0.6, 0.1], {"title": "Backups and restore", "topic": "ops"}),
]


def main() -> None:
    with Client(URL, api_key=API_KEY) as q:
        try:
            q.delete_collection("demo")
        except Exception:  # noqa: BLE001 - best-effort reset on re-run
            pass
        q.create_collection("demo", dim=4, metric="cosine")
        q.upsert("demo", DEMO)
        nearest = q.search("demo", [0.9, 0.1, 0.0, 0.0], k=3)
        print(f"seeded {len(DEMO)} points into 'demo' (encrypted at rest)")
        print("sample query -> " + ", ".join(f"{h.id}({h.payload['topic']})" for h in nearest))


if __name__ == "__main__":
    main()
