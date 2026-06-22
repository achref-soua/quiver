# SPDX-License-Identifier: AGPL-3.0-only
"""A Haystack 2.x ``DocumentStore`` backed by Quiver, with a companion retriever.

Quiver stores each Haystack ``Document``'s embedding as the vector and its
``content``/``meta`` in the payload, so Haystack metadata filters map onto
Quiver's exact payload pre-filter (``meta.x`` is a dot-path into the payload).
Any Quiver index — including the memory-frugal disk path — backs the store.

Example::

    from quiver import Client
    from quiver.haystack import QuiverDocumentStore, QuiverEmbeddingRetriever

    store = QuiverDocumentStore(Client(api_key="…"), "docs", dim=384, metric="cosine")
    store.write_documents(documents)             # documents carry .embedding
    retriever = QuiverEmbeddingRetriever(store)
    hits = retriever.run(query_embedding=embedding, top_k=5)["documents"]

Install with ``pip install "quiver-client[haystack]"``.
"""

from __future__ import annotations

from typing import Any, Optional

from haystack import Document, component, default_to_dict
from haystack.document_stores.types import DuplicatePolicy

from .client import TEXT_KEY, Client, FilterableField, Point

__all__ = ["QuiverDocumentStore", "QuiverEmbeddingRetriever"]


class QuiverDocumentStore:
    """A Haystack ``DocumentStore`` over a Quiver collection.

    The collection is created lazily on the first write (its dimension inferred
    from the first document's embedding) unless it already exists.
    """

    def __init__(
        self,
        client: Client,
        collection: str,
        *,
        dim: Optional[int] = None,
        metric: str = "cosine",
        index: Optional[str] = None,
        pq_subspaces: Optional[int] = None,
        filterable: Optional[list[FilterableField]] = None,
        hybrid: bool = False,
    ) -> None:
        self._client = client
        self._collection = collection
        self._dim = dim
        self._metric = metric
        self._index = index
        self._pq_subspaces = pq_subspaces
        self._filterable = list(filterable) if filterable else []
        # When True, index each document's content for BM25 and fuse dense ⊕ BM25
        # at retrieval when a query text is supplied (ADR-0043/0046).
        self._hybrid = hybrid

    # --- Haystack DocumentStore protocol ---

    def count_documents(self) -> int:
        try:
            return int(self._client.get_collection(self._collection).count)
        except Exception:  # noqa: BLE001 - collection not created yet
            return 0

    def write_documents(
        self,
        documents: list[Document],
        policy: DuplicatePolicy = DuplicatePolicy.NONE,
    ) -> int:
        """Upsert documents (Quiver upsert is replace-by-id, i.e. OVERWRITE)."""
        if not documents:
            return 0
        first = next((d for d in documents if d.embedding is not None), None)
        if first is None:
            raise ValueError("QuiverDocumentStore requires documents with embeddings")
        self._ensure_collection(len(first.embedding))
        points = [
            Point(id=doc.id, vector=list(doc.embedding), payload=self._payload(doc))
            for doc in documents
            if doc.embedding is not None
        ]
        return self._client.upsert(self._collection, points)

    def _payload(self, doc: Document) -> dict[str, Any]:
        payload: dict[str, Any] = {"content": doc.content, "meta": doc.meta or {}}
        if self._hybrid and doc.content:
            payload[TEXT_KEY] = doc.content  # index the content for BM25 (ADR-0046)
        return payload

    def filter_documents(self, filters: Optional[dict[str, Any]] = None) -> list[Document]:
        """Return documents matching ``filters`` (no ranking), via Quiver fetch."""
        matches = self._client.fetch(
            self._collection,
            filter=_to_quiver_filter(filters),
            limit=10_000,
            with_payload=True,
            with_vector=True,
        )
        return [_to_haystack_document(m.id, m.payload, m.vector, score=None) for m in matches]

    def delete_documents(self, document_ids: list[str]) -> None:
        if document_ids:
            self._client.delete_points(self._collection, document_ids)

    def to_dict(self) -> dict[str, Any]:
        return default_to_dict(
            self,
            collection=self._collection,
            dim=self._dim,
            metric=self._metric,
            index=self._index,
            pq_subspaces=self._pq_subspaces,
        )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "QuiverDocumentStore":
        # The Client is not serializable; reconstruct a default one. Callers that
        # need auth should build the store directly rather than via from_dict.
        params = data.get("init_parameters", {})
        params.pop("client", None)
        return cls(Client(), **params)

    # --- retrieval (used by QuiverEmbeddingRetriever) ---

    def _embedding_retrieval(
        self,
        query_embedding: list[float],
        *,
        filters: Optional[dict[str, Any]] = None,
        top_k: int = 10,
        query_text: Optional[str] = None,
    ) -> list[Document]:
        filter_ = _to_quiver_filter(filters)
        if self._hybrid and query_text:
            # Fuse the dense neighbours with a BM25 query over the indexed content.
            hits = self._client.hybrid_search(
                self._collection,
                vector=query_embedding,
                query_text=query_text,
                k=top_k,
                filter=filter_,
                with_payload=True,
            )
        else:
            hits = self._client.search(
                self._collection, query_embedding, k=top_k, filter=filter_, with_payload=True
            )
        return [_to_haystack_document(h.id, h.payload, None, score=h.score) for h in hits]

    def _ensure_collection(self, dim: int) -> None:
        try:
            self._client.get_collection(self._collection)
            return
        except Exception:  # noqa: BLE001 - not created yet
            pass
        self._client.create_collection(
            self._collection,
            dim=self._dim or dim,
            metric=self._metric,
            index=self._index,
            pq_subspaces=self._pq_subspaces,
            filterable=self._filterable or None,
        )


@component
class QuiverEmbeddingRetriever:
    """A Haystack retriever component over a :class:`QuiverDocumentStore`."""

    def __init__(
        self,
        document_store: QuiverDocumentStore,
        *,
        filters: Optional[dict[str, Any]] = None,
        top_k: int = 10,
    ) -> None:
        self._store = document_store
        self._filters = filters
        self._top_k = top_k

    @component.output_types(documents=list[Document])
    def run(
        self,
        query_embedding: list[float],
        filters: Optional[dict[str, Any]] = None,
        top_k: Optional[int] = None,
        query_text: Optional[str] = None,
    ) -> dict[str, list[Document]]:
        # Pass ``query_text`` (with a hybrid store) to also score BM25 keywords.
        docs = self._store._embedding_retrieval(
            query_embedding,
            filters=filters if filters is not None else self._filters,
            top_k=top_k if top_k is not None else self._top_k,
            query_text=query_text,
        )
        return {"documents": docs}


def _to_haystack_document(
    id: str, payload: Any, vector: Optional[list[float]], *, score: Optional[float]
) -> Document:
    payload = payload or {}
    return Document(
        id=id,
        content=payload.get("content"),
        meta=payload.get("meta", {}),
        embedding=vector,
        score=score,
    )


# Haystack 2.x filter operators → Quiver payload-filter leaves (the quiver-query
# wire shape). Logical nodes use {"operator": "AND"|"OR"|"NOT", "conditions": [...]}.
_LEAF_OPS = {
    "==": "eq",
    "!=": "ne",
    ">": "gt",
    ">=": "gte",
    "<": "lt",
    "<=": "lte",
    "in": "in",
}


def _to_quiver_filter(filters: Optional[dict[str, Any]]) -> Optional[dict[str, Any]]:
    """Translate a Haystack 2.x filter dict into a Quiver filter, or ``None``."""
    if not filters:
        return None
    op = filters.get("operator")
    if op in ("AND", "OR", "NOT"):
        conditions = [
            f for f in (_to_quiver_filter(c) for c in filters.get("conditions", [])) if f
        ]
        if not conditions:
            return None
        if op == "AND":
            return {"and": conditions}
        if op == "OR":
            return {"or": conditions}
        return {"not": conditions[0] if len(conditions) == 1 else {"and": conditions}}
    # Leaf comparison.
    field = filters.get("field", "")
    value = filters.get("value")
    if op == "in":
        return {"in": {"field": field, "values": list(value or [])}}
    if op == "not in":
        return {"not": {"in": {"field": field, "values": list(value or [])}}}
    quiver_op = _LEAF_OPS.get(op)
    if quiver_op is None:
        raise ValueError(f"unsupported Haystack filter operator: {op!r}")
    return {quiver_op: {"field": field, "value": value}}
