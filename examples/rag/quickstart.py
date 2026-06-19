#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""End-to-end RAG with Quiver — chunk → embed → upsert → filtered search → answer.

This script is intentionally dependency-light: it uses a tiny *deterministic*
hash embedder so it runs with no API key or model download, against a local
`quiver serve`. Swap `embed()` for a real model (sentence-transformers, OpenAI,
Cohere, …) for production — Quiver is model-agnostic and just stores the vectors
you give it.

Run:
    # 1. start an insecure local server (dev only)
    QUIVER_INSECURE=true QUIVER_API_KEYS=dev cargo run --release -p quiver-cli -- serve &
    # 2. run this script
    python examples/rag/quickstart.py
"""

from __future__ import annotations

import hashlib
import os
import re

from quiver import Client, FilterableField, Point

DIM = 256
COLLECTION = "kb"
URL = os.environ.get("QUIVER_URL", "http://127.0.0.1:6333")
API_KEY = os.environ.get("QUIVER_API_KEY", "dev")

# A small knowledge base: (text, metadata). In a real app these come from your
# documents, split into chunks (see chunk() below).
DOCS = [
    ("Quiver encrypts every durable byte at rest with XChaCha20-Poly1305.", {"topic": "security", "year": 2026}),
    ("The disk-resident DiskVamana index keeps only PQ codes in RAM.", {"topic": "indexing", "year": 2026}),
    ("Quiver exposes an MCP server so agents can drive it over stdio.", {"topic": "agents", "year": 2026}),
    ("Client-side vector encryption lets the server store blobs it cannot read.", {"topic": "security", "year": 2025}),
    ("HNSW gives the highest recall for small, hot, in-memory collections.", {"topic": "indexing", "year": 2025}),
]


def embed(text: str) -> list[float]:
    """A deterministic bag-of-words hash embedding — NO model needed.

    Replace with a real embedder in production, e.g.::

        from sentence_transformers import SentenceTransformer
        model = SentenceTransformer("all-MiniLM-L6-v2")  # dim=384
        def embed(text): return model.encode(text).tolist()
    """
    vec = [0.0] * DIM
    for token in re.findall(r"[a-z0-9]+", text.lower()):
        h = int.from_bytes(hashlib.sha256(token.encode()).digest()[:4], "big")
        vec[h % DIM] += 1.0
    norm = sum(x * x for x in vec) ** 0.5 or 1.0
    return [x / norm for x in vec]


def chunk(text: str, *, size: int = 400, overlap: int = 80) -> list[str]:
    """Split long text into overlapping character windows (toy splitter)."""
    if len(text) <= size:
        return [text]
    out, start = [], 0
    while start < len(text):
        out.append(text[start : start + size])
        start += size - overlap
    return out


def main() -> None:
    with Client(URL, api_key=API_KEY) as q:
        # 1. A collection with a filterable `topic` (keyword) and `year` (numeric),
        #    so retrieval can be scoped — the metadata pre-filter is exact.
        try:
            q.delete_collection(COLLECTION)
        except Exception:  # noqa: BLE001 - first run
            pass
        q.create_collection(
            COLLECTION,
            dim=DIM,
            metric="cosine",
            filterable=[FilterableField("topic", "keyword"), FilterableField("year", "numeric")],
        )

        # 2. Embed + upsert. upsert_iter batches large corpora and reports progress.
        points = [
            Point(id=f"doc-{i}", vector=embed(text), payload={"text": text, **meta})
            for i, (text, meta) in enumerate(DOCS)
        ]
        n = q.upsert_iter(COLLECTION, points, batch=500, on_progress=lambda t: print(f"  upserted {t}"))
        print(f"indexed {n} chunks")

        # 3. Retrieve: nearest neighbours, scoped to recent security docs only.
        question = "How does Quiver protect data at rest?"
        hits = q.search(
            COLLECTION,
            embed(question),
            k=3,
            filter={"and": [
                {"eq": {"field": "topic", "value": "security"}},
                {"gte": {"field": "year", "value": 2026}},
            ]},
        )

        # 4. Rerank locally (here: keep the top hit) and build the LLM context.
        #    In production, hand `context` to your LLM as grounding.
        context = "\n".join(f"- {h.payload['text']}" for h in hits)
        print(f"\nQ: {question}\nContext for the LLM:\n{context}")
        print("\n(Feed `context` + the question to your LLM of choice to generate the answer.)")


if __name__ == "__main__":
    main()
