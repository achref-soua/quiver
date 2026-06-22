# SPDX-License-Identifier: AGPL-3.0-only
"""A LlamaIndex ``VectorStore`` backed by Quiver.

Optional integration — requires ``llama-index-core`` (install
``quiver-client[llamaindex]``). Nodes (already embedded by LlamaIndex) are
upserted into a Quiver collection and retrieved by a [`VectorStoreQuery`];
LlamaIndex ``MetadataFilters`` are translated into Quiver payload filters, so a
filtered retriever uses Quiver's hybrid pre-filter path. Any Quiver index backs
the store, including the memory-frugal ``disk_vamana`` path.

    from quiver import Client, FilterableField
    from quiver.llamaindex import QuiverVectorStore
    from llama_index.core import StorageContext, VectorStoreIndex

    store = QuiverVectorStore(
        Client(api_key="…"), "docs",
        index="disk_vamana", pq_subspaces=48,
        filterable=[FilterableField("category", "keyword")],
    )
    ctx = StorageContext.from_defaults(vector_store=store)
    VectorStoreIndex(nodes, storage_context=ctx)
"""

from __future__ import annotations

from typing import Any, Optional, Sequence

from llama_index.core.bridge.pydantic import PrivateAttr
from llama_index.core.schema import BaseNode, TextNode
from llama_index.core.vector_stores.types import (
    BasePydanticVectorStore,
    FilterCondition,
    FilterOperator,
    MetadataFilter,
    MetadataFilters,
    VectorStoreQuery,
    VectorStoreQueryResult,
)
from llama_index.core.vector_stores.utils import (
    metadata_dict_to_node,
    node_to_metadata_dict,
)

from .client import TEXT_KEY, Client, FilterableField, Point, QuiverError

__all__ = ["QuiverVectorStore"]


class QuiverVectorStore(BasePydanticVectorStore):
    """A LlamaIndex ``VectorStore`` over a single Quiver collection.

    The collection is created on first ``add`` if it does not exist (dimension
    inferred from the first node's embedding), unless ``create_collection`` is
    ``False``. ``delete`` removes the point with the given id; ``delete_nodes``
    removes explicit node ids — Quiver deletes by id.
    """

    stores_text: bool = True
    flat_metadata: bool = False

    collection_name: str
    text_key: str = "text"
    metric: str = "cosine"
    index: Optional[str] = None
    pq_subspaces: Optional[int] = None
    create_collection: bool = True
    # When True, also index each node's text for BM25 and fuse dense ⊕ BM25 at
    # query time via RRF (ADR-0043/0046).
    hybrid: bool = False

    _client: Client = PrivateAttr()
    _filterable: list[FilterableField] = PrivateAttr(default_factory=list)
    _ensured: bool = PrivateAttr(default=False)

    def __init__(
        self,
        client: Client,
        collection: str,
        *,
        text_key: str = "text",
        metric: str = "cosine",
        index: Optional[str] = None,
        pq_subspaces: Optional[int] = None,
        filterable: Optional[Sequence[FilterableField]] = None,
        create_collection: bool = True,
        hybrid: bool = False,
        **kwargs: Any,
    ) -> None:
        super().__init__(
            collection_name=collection,
            text_key=text_key,
            metric=metric,
            index=index,
            pq_subspaces=pq_subspaces,
            create_collection=create_collection,
            hybrid=hybrid,
            **kwargs,
        )
        self._client = client
        self._filterable = list(filterable) if filterable else []

    @classmethod
    def class_name(cls) -> str:
        return "QuiverVectorStore"

    @property
    def client(self) -> Any:
        return self._client

    def add(self, nodes: Sequence[BaseNode], **_kwargs: Any) -> list[str]:
        node_list = list(nodes)
        if not node_list:
            return []
        self._ensure_collection(len(node_list[0].get_embedding()))
        points: list[Point] = []
        ids: list[str] = []
        for node in node_list:
            payload = node_to_metadata_dict(
                node, remove_text=False, flat_metadata=self.flat_metadata
            )
            if self.hybrid:
                # Index the node's text for BM25 keyword search (ADR-0046).
                payload[TEXT_KEY] = node.get_content()
            points.append(
                Point(
                    id=node.node_id,
                    vector=list(node.get_embedding()),
                    payload=payload,
                )
            )
            ids.append(node.node_id)
        self._client.upsert(self.collection_name, points)
        return ids

    def delete(self, ref_doc_id: str, **_kwargs: Any) -> None:
        self._client.delete_points(self.collection_name, [ref_doc_id])

    def delete_nodes(
        self,
        node_ids: Optional[list[str]] = None,
        filters: Optional[MetadataFilters] = None,
        **_kwargs: Any,
    ) -> None:
        if node_ids:
            self._client.delete_points(self.collection_name, list(node_ids))

    def query(self, query: VectorStoreQuery, **_kwargs: Any) -> VectorStoreQueryResult:
        embedding = list(query.query_embedding or [])
        filter_ = _to_quiver_filter(query.filters)
        k = query.similarity_top_k
        if self.hybrid and query.query_str:
            # Fuse the dense neighbours with a BM25 query over the indexed text.
            matches = self._client.hybrid_search(
                self.collection_name,
                vector=embedding,
                query_text=query.query_str,
                k=k,
                filter=filter_,
            )
        else:
            matches = self._client.search(self.collection_name, embedding, k=k, filter=filter_)
        nodes: list[BaseNode] = []
        similarities: list[float] = []
        ids: list[str] = []
        for match in matches:
            payload = dict(match.payload or {})
            payload.pop(TEXT_KEY, None)  # internal BM25 field, never a node metadata
            try:
                node = metadata_dict_to_node(payload)
            except ValueError:
                # No serialized node content — fall back to the raw text payload.
                node = TextNode(
                    id_=match.id,
                    text=str(payload.get(self.text_key, "")),
                    metadata=payload,
                )
            nodes.append(node)
            similarities.append(match.score)
            ids.append(match.id)
        return VectorStoreQueryResult(nodes=nodes, similarities=similarities, ids=ids)

    # Create the backing collection on first write if it is absent.
    def _ensure_collection(self, dim: int) -> None:
        if self._ensured:
            return
        if self.create_collection:
            try:
                self._client.get_collection(self.collection_name)
            except QuiverError:
                self._client.create_collection(
                    self.collection_name,
                    dim,
                    self.metric,
                    index=self.index,
                    pq_subspaces=self.pq_subspaces,
                    filterable=self._filterable or None,
                )
        self._ensured = True


# Translate LlamaIndex metadata filters into a Quiver payload-filter dict (the
# `quiver-query` Filter wire shape). `None` when there is nothing to filter.
def _to_quiver_filter(filters: Optional[MetadataFilters]) -> Optional[dict[str, Any]]:
    if filters is None or not filters.filters:
        return None
    return _filters_to_dict(filters)


def _filters_to_dict(filters: MetadataFilters) -> dict[str, Any]:
    parts: list[dict[str, Any]] = []
    for sub in filters.filters:
        if isinstance(sub, MetadataFilters):
            parts.append(_filters_to_dict(sub))
        else:
            parts.append(_filter_to_dict(sub))
    condition = "or" if filters.condition == FilterCondition.OR else "and"
    return {condition: parts}


def _filter_to_dict(f: MetadataFilter) -> dict[str, Any]:
    field = f.key
    simple = {
        FilterOperator.EQ: "eq",
        FilterOperator.NE: "ne",
        FilterOperator.GT: "gt",
        FilterOperator.GTE: "gte",
        FilterOperator.LT: "lt",
        FilterOperator.LTE: "lte",
    }
    if f.operator in simple:
        return {simple[f.operator]: {"field": field, "value": f.value}}
    if f.operator == FilterOperator.IN:
        return {"in": {"field": field, "values": list(f.value)}}
    raise ValueError(f"unsupported LlamaIndex filter operator: {f.operator}")
