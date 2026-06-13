# SPDX-License-Identifier: AGPL-3.0-only
"""A LangChain ``VectorStore`` backed by Quiver.

Optional integration — requires ``langchain-core`` (install
``quiver-client[langchain]``). Texts are embedded by a caller-supplied
``Embeddings``, upserted into a Quiver collection (the text stored under a
payload key), and retrieved by embedding the query. Any Quiver index backs the
retriever, including the memory-frugal ``disk_vamana`` path.

    from quiver import Client
    from quiver.langchain import QuiverVectorStore

    store = QuiverVectorStore.from_texts(
        texts, embedding, client=Client(api_key="…"),
        collection="docs", index="disk_vamana", pq_subspaces=48,
    )
    docs = store.similarity_search("query", k=4)
"""

from __future__ import annotations

from typing import Any, Iterable, Optional
from uuid import uuid4

from langchain_core.documents import Document
from langchain_core.embeddings import Embeddings
from langchain_core.vectorstores import VectorStore

from .client import Client, Match, Point

__all__ = ["QuiverVectorStore"]


class QuiverVectorStore(VectorStore):
    """A LangChain ``VectorStore`` over a single Quiver collection."""

    def __init__(
        self,
        client: Client,
        collection: str,
        embedding: Embeddings,
        *,
        text_key: str = "text",
    ) -> None:
        self._client = client
        self._collection = collection
        self._embedding = embedding
        self._text_key = text_key

    @property
    def embeddings(self) -> Embeddings:
        return self._embedding

    def add_texts(
        self,
        texts: Iterable[str],
        metadatas: Optional[list[dict[str, Any]]] = None,
        *,
        ids: Optional[list[str]] = None,
        **_kwargs: Any,
    ) -> list[str]:
        items = list(texts)
        vectors = self._embedding.embed_documents(items)
        out_ids = list(ids) if ids is not None else [str(uuid4()) for _ in items]
        metas = list(metadatas) if metadatas is not None else [{} for _ in items]
        points = [
            Point(id=id_, vector=list(vector), payload={**meta, self._text_key: text})
            for id_, text, vector, meta in zip(out_ids, items, vectors, metas)
        ]
        if points:
            self._client.upsert(self._collection, points)
        return out_ids

    def delete(self, ids: Optional[list[str]] = None, **_kwargs: Any) -> Optional[bool]:
        if not ids:
            return None
        self._client.delete_points(self._collection, list(ids))
        return True

    def similarity_search(self, query: str, k: int = 4, **kwargs: Any) -> list[Document]:
        return [doc for doc, _ in self.similarity_search_with_score(query, k, **kwargs)]

    def similarity_search_with_score(
        self,
        query: str,
        k: int = 4,
        *,
        filter: Optional[dict[str, Any]] = None,
        **_kwargs: Any,
    ) -> list[tuple[Document, float]]:
        vector = self._embedding.embed_query(query)
        matches = self._client.search(self._collection, list(vector), k=k, filter=filter)
        return [(self._to_document(m), m.score) for m in matches]

    def _to_document(self, match: Match) -> Document:
        payload = dict(match.payload or {})
        text = payload.pop(self._text_key, "")
        return Document(page_content=str(text), metadata=payload, id=match.id)

    @classmethod
    def from_texts(
        cls,
        texts: list[str],
        embedding: Embeddings,
        metadatas: Optional[list[dict[str, Any]]] = None,
        *,
        client: Client,
        collection: str,
        dim: Optional[int] = None,
        metric: str = "cosine",
        index: Optional[str] = None,
        pq_subspaces: Optional[int] = None,
        create: bool = True,
        text_key: str = "text",
        ids: Optional[list[str]] = None,
        **_kwargs: Any,
    ) -> "QuiverVectorStore":
        items = list(texts)
        if create:
            resolved_dim = dim if dim is not None else len(embedding.embed_query(items[0] if items else " "))
            client.create_collection(
                collection, resolved_dim, metric, index=index, pq_subspaces=pq_subspaces
            )
        store = cls(client, collection, embedding, text_key=text_key)
        store.add_texts(items, metadatas, ids=ids)
        return store
