# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for the LlamaIndex VectorStore adapter, against a fake Quiver client."""

from __future__ import annotations

from typing import Any

import pytest
from llama_index.core.schema import MetadataMode, TextNode
from llama_index.core.vector_stores.types import (
    FilterCondition,
    FilterOperator,
    MetadataFilter,
    MetadataFilters,
    VectorStoreQuery,
)

from quiver.client import FilterableField, Match, Point, QuiverError
from quiver.llamaindex import QuiverVectorStore, _to_quiver_filter


class FakeClient:
    """An in-memory stand-in for quiver.Client recording the adapter's calls."""

    def __init__(self, *, exists: bool = False) -> None:
        self.created: list[dict[str, Any]] = []
        self.points: dict[str, dict[str, Point]] = {}
        self.deleted: list[list[str]] = []
        self.last_filter: Any = None
        self._exists = exists

    def get_collection(self, name: str) -> dict[str, Any]:
        if not self._exists:
            raise QuiverError(f"collection {name} not found", status=404)
        return {"name": name}

    def create_collection(
        self,
        name: str,
        dim: int,
        metric: str = "l2",
        *,
        index: Any = None,
        pq_subspaces: Any = None,
        filterable: Any = None,
    ) -> None:
        self.created.append(
            {
                "name": name,
                "dim": dim,
                "metric": metric,
                "index": index,
                "pq_subspaces": pq_subspaces,
                "filterable": list(filterable) if filterable else [],
            }
        )

    def upsert(self, collection: str, points: list[Point]) -> int:
        store = self.points.setdefault(collection, {})
        pts = list(points)
        for p in pts:
            store[p.id] = p
        return len(pts)

    def search(
        self, collection: str, vector: list[float], *, k: int = 10, filter: Any = None
    ) -> list[Match]:
        self.last_filter = filter
        pts = list(self.points.get(collection, {}).values())[:k]
        return [Match(id=p.id, score=0.5, payload=p.payload, vector=None) for p in pts]

    def hybrid_search(
        self, collection: str, *, vector: Any = None, query_text: Any = None, k: int = 10, filter: Any = None
    ) -> list[Match]:
        self.last_query_text = query_text
        pts = list(self.points.get(collection, {}).values())[:k]
        return [Match(id=p.id, score=0.6, payload=p.payload, vector=None) for p in pts]

    def delete_points(self, collection: str, ids: list[str]) -> int:
        self.deleted.append(list(ids))
        store = self.points.get(collection, {})
        return sum(1 for i in ids if store.pop(i, None) is not None)


def _node(node_id: str, text: str, **metadata: Any) -> TextNode:
    node = TextNode(id_=node_id, text=text, metadata=metadata)
    node.embedding = [float(len(text)), float(text.count("a")), 1.0]
    return node


def test_add_creates_collection_and_query_reconstructs_nodes() -> None:
    client = FakeClient()
    store = QuiverVectorStore(
        client,
        "docs",
        index="disk_vamana",
        pq_subspaces=1,
        filterable=[FilterableField("category", "keyword")],
    )
    ids = store.add(
        [_node("n1", "alpha", category="x"), _node("n2", "beta", category="y")]
    )
    assert ids == ["n1", "n2"]
    # The collection was created lazily, inferring the dim and forwarding knobs.
    assert client.created[0]["dim"] == 3
    assert client.created[0]["index"] == "disk_vamana"
    assert client.created[0]["pq_subspaces"] == 1
    assert client.created[0]["filterable"] == [FilterableField("category", "keyword")]

    result = store.query(
        VectorStoreQuery(query_embedding=[1.0, 0.0, 1.0], similarity_top_k=2)
    )
    texts = {n.get_content(metadata_mode=MetadataMode.NONE) for n in result.nodes}
    assert texts == {"alpha", "beta"}
    assert result.ids == ["n1", "n2"]
    # Metadata survives the round-trip through the payload.
    assert {n.metadata.get("category") for n in result.nodes} == {"x", "y"}


def test_hybrid_mode_indexes_text_and_fuses_bm25() -> None:
    from quiver.client import TEXT_KEY

    client = FakeClient(exists=True)
    store = QuiverVectorStore(client, "docs", hybrid=True)
    store.add([_node("n1", "quick brown fox", category="x")])
    # Hybrid ingest co-populates the reserved BM25 key with the node text.
    assert client.points["docs"]["n1"].payload[TEXT_KEY] == "quick brown fox"

    result = store.query(
        VectorStoreQuery(query_embedding=[1.0, 0.0, 1.0], similarity_top_k=1, query_str="quick fox")
    )
    # The query routed through hybrid_search with the raw text for BM25 fusion.
    assert client.last_query_text == "quick fox"
    # The internal BM25 field never leaks into node metadata.
    assert TEXT_KEY not in result.nodes[0].metadata


def test_existing_collection_is_not_recreated_and_deletes_by_id() -> None:
    client = FakeClient(exists=True)
    store = QuiverVectorStore(client, "docs")
    store.add([_node("n1", "alpha")])
    assert client.created == []  # already existed, so not recreated
    store.delete("n1")
    store.delete_nodes(node_ids=["n2", "n3"])
    assert client.deleted == [["n1"], ["n2", "n3"]]


def test_query_translates_metadata_filters() -> None:
    client = FakeClient(exists=True)
    store = QuiverVectorStore(client, "docs")
    store.add([_node("n1", "alpha", category="x")])
    filters = MetadataFilters(
        filters=[
            MetadataFilter(key="category", value="x", operator=FilterOperator.EQ),
            MetadataFilter(key="score", value=3, operator=FilterOperator.GTE),
        ],
        condition=FilterCondition.AND,
    )
    store.query(
        VectorStoreQuery(
            query_embedding=[1.0, 0.0, 1.0], similarity_top_k=5, filters=filters
        )
    )
    assert client.last_filter == {
        "and": [
            {"eq": {"field": "category", "value": "x"}},
            {"gte": {"field": "score", "value": 3}},
        ]
    }


def test_filter_translator_handles_in_or_and_rejects_unsupported() -> None:
    or_filters = MetadataFilters(
        filters=[
            MetadataFilter(
                key="city", value=["paris", "lyon"], operator=FilterOperator.IN
            ),
            MetadataFilter(key="tier", value="free", operator=FilterOperator.NE),
        ],
        condition=FilterCondition.OR,
    )
    assert _to_quiver_filter(or_filters) == {
        "or": [
            {"in": {"field": "city", "values": ["paris", "lyon"]}},
            {"ne": {"field": "tier", "value": "free"}},
        ]
    }
    assert _to_quiver_filter(None) is None
    # An operator Quiver's filter cannot express is a clear error.
    bad = MetadataFilters(
        filters=[
            MetadataFilter(key="t", value="x", operator=FilterOperator.TEXT_MATCH)
        ]
    )
    with pytest.raises(ValueError):
        _to_quiver_filter(bad)
