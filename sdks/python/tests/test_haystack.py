# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the Haystack DocumentStore adapter, against a fake Quiver client."""

from __future__ import annotations

from typing import Any

from haystack import Document

from quiver.client import CollectionInfo, Match, Point
from quiver.haystack import QuiverDocumentStore, QuiverEmbeddingRetriever, _to_quiver_filter


class FakeClient:
    """Records calls and returns canned data — no server."""

    def __init__(self) -> None:
        self.upserted: list[Point] = []
        self.created = False
        self.searched: dict[str, Any] = {}
        self.fetched: dict[str, Any] = {}
        self.deleted: list[str] = []

    def get_collection(self, name: str) -> CollectionInfo:
        if not self.created:
            raise RuntimeError("not found")
        return CollectionInfo(name=name, dim=3, metric="cosine", count=len(self.upserted))

    def create_collection(self, name: str, dim: int, metric: str = "l2", **_kw: Any) -> CollectionInfo:
        self.created = True
        return CollectionInfo(name=name, dim=dim, metric=metric, count=0)

    def upsert(self, collection: str, points: list[Point]) -> int:
        self.upserted.extend(points)
        return len(points)

    def search(self, collection: str, vector, **kwargs: Any) -> list[Match]:
        self.searched = {"vector": vector, **kwargs}
        return [Match(id="a", score=0.9, payload={"content": "hi", "meta": {"y": 2024}})]

    def fetch(self, collection: str, **kwargs: Any) -> list[Match]:
        self.fetched = kwargs
        return [Match(id="b", score=0.0, payload={"content": "yo", "meta": {}}, vector=[0.1, 0.2, 0.3])]

    def delete_points(self, collection: str, ids: list[str]) -> int:
        self.deleted.extend(ids)
        return len(ids)


def test_write_creates_collection_and_upserts_embeddings():
    c = FakeClient()
    store = QuiverDocumentStore(c, "docs", metric="cosine")
    n = store.write_documents([Document(content="hi", meta={"y": 2024}, embedding=[0.1, 0.2, 0.3])])
    assert n == 1 and c.created
    assert c.upserted[0].vector == [0.1, 0.2, 0.3]
    assert c.upserted[0].payload == {"content": "hi", "meta": {"y": 2024}}


def test_embedding_retrieval_returns_documents_with_scores():
    c = FakeClient()
    c.created = True
    store = QuiverDocumentStore(c, "docs")
    docs = store._embedding_retrieval([0.1, 0.2, 0.3], top_k=5)
    assert docs[0].content == "hi" and docs[0].score == 0.9 and docs[0].meta == {"y": 2024}
    assert c.searched["k"] == 5


def test_retriever_component_runs():
    c = FakeClient()
    c.created = True
    store = QuiverDocumentStore(c, "docs")
    out = QuiverEmbeddingRetriever(store, top_k=3).run(query_embedding=[0.0, 0.0, 0.0])
    assert [d.id for d in out["documents"]] == ["a"]


def test_filter_documents_uses_fetch():
    c = FakeClient()
    c.created = True
    store = QuiverDocumentStore(c, "docs")
    docs = store.filter_documents({"field": "meta.y", "operator": ">=", "value": 2020})
    assert docs[0].id == "b"
    assert c.fetched["filter"] == {"gte": {"field": "meta.y", "value": 2020}}


def test_filter_translation_logical_and_leaf():
    f = {
        "operator": "AND",
        "conditions": [
            {"field": "meta.lang", "operator": "==", "value": "en"},
            {"field": "meta.tag", "operator": "in", "value": ["a", "b"]},
            {"operator": "NOT", "conditions": [{"field": "meta.x", "operator": "<", "value": 5}]},
        ],
    }
    assert _to_quiver_filter(f) == {
        "and": [
            {"eq": {"field": "meta.lang", "value": "en"}},
            {"in": {"field": "meta.tag", "values": ["a", "b"]}},
            {"not": {"lt": {"field": "meta.x", "value": 5}}},
        ]
    }
    assert _to_quiver_filter(None) is None


def test_delete_documents():
    c = FakeClient()
    c.created = True
    store = QuiverDocumentStore(c, "docs")
    store.delete_documents(["x", "y"])
    assert c.deleted == ["x", "y"]
