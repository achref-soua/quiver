# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the LangChain VectorStore adapter, against a fake Quiver client."""

from __future__ import annotations

from typing import Any, Optional

from langchain_core.embeddings import Embeddings

from quiver.client import Match, Point
from quiver.langchain import QuiverVectorStore


class FakeEmbeddings(Embeddings):
    """A tiny deterministic embedding so tests need no model."""

    def embed_documents(self, texts: list[str]) -> list[list[float]]:
        return [self._vec(t) for t in texts]

    def embed_query(self, text: str) -> list[float]:
        return self._vec(text)

    @staticmethod
    def _vec(text: str) -> list[float]:
        return [float(len(text)), float(text.count("a")), float(text.count("e"))]


class FakeClient:
    """An in-memory stand-in for quiver.Client recording the calls the adapter makes."""

    def __init__(self) -> None:
        self.created: list[dict[str, Any]] = []
        self.points: dict[str, dict[str, Point]] = {}

    def create_collection(
        self, name: str, dim: int, metric: str = "l2", *, index: Optional[str] = None, pq_subspaces: Optional[int] = None
    ) -> None:
        self.created.append({"name": name, "dim": dim, "metric": metric, "index": index, "pq_subspaces": pq_subspaces})

    def upsert(self, collection: str, points: list[Point]) -> int:
        pts = list(points)
        store = self.points.setdefault(collection, {})
        for p in pts:
            store[p.id] = p
        return len(pts)

    def search(self, collection: str, vector: list[float], *, k: int = 10, filter: Any = None) -> list[Match]:
        pts = list(self.points.get(collection, {}).values())[:k]
        return [Match(id=p.id, score=0.9, payload=p.payload, vector=None) for p in pts]

    def delete_points(self, collection: str, ids: list[str]) -> int:
        store = self.points.get(collection, {})
        return sum(1 for i in ids if store.pop(i, None) is not None)


def test_from_texts_creates_with_index_and_reconstructs_documents() -> None:
    client = FakeClient()
    store = QuiverVectorStore.from_texts(
        ["alpha", "beta"],
        FakeEmbeddings(),
        metadatas=[{"src": "a"}, {"src": "b"}],
        client=client,
        collection="docs",
        metric="cosine",
        index="disk_vamana",
        pq_subspaces=1,
    )
    assert client.created[0]["index"] == "disk_vamana"
    assert client.created[0]["pq_subspaces"] == 1
    assert client.created[0]["metric"] == "cosine"
    assert client.created[0]["dim"] == 3  # inferred from the embedding

    docs = store.similarity_search("alpha", k=2)
    assert {d.page_content for d in docs} == {"alpha", "beta"}
    for d in docs:
        assert "text" not in d.metadata  # the text_key is stripped back out
        assert d.metadata.get("src") in {"a", "b"}


def test_add_texts_stores_text_in_payload_scores_and_deletes() -> None:
    client = FakeClient()
    store = QuiverVectorStore(client, "c", FakeEmbeddings())
    ids = store.add_texts(["x", "y"], [{"k": 1}, {"k": 2}], ids=["i1", "i2"])
    assert ids == ["i1", "i2"]
    assert client.points["c"]["i1"].payload == {"k": 1, "text": "x"}

    scored = store.similarity_search_with_score("x", k=2)
    assert all(score == 0.9 for _, score in scored)

    assert store.delete(["i1"]) is True
    assert "i1" not in client.points["c"]
    assert store.delete([]) is None
