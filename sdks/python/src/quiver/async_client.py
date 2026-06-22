# SPDX-License-Identifier: AGPL-3.0-only
"""An asynchronous REST client for Quiver.

`AsyncClient` mirrors the synchronous :class:`quiver.client.Client` over the same
HTTP contract (``docs/api/rest-grpc.md``), for RAG services and agents that issue
many concurrent retrievals. It reuses the sync module's pure request/response
helpers so the two clients cannot drift, and adds a few ergonomic helpers
(``delete_by_filter``, ``scroll``, ``upsert_iter``) that are also available on the
sync client.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, AsyncIterator, Awaitable, Callable, Iterable, Mapping, Optional, Sequence

import httpx

from .client import (
    DEFAULT_BASE_URL,
    DEFAULT_TIMEOUT,
    CollectionInfo,
    Document,
    DocumentMatch,
    FilterableField,
    Match,
    PointInput,
    QuiverError,
    SparseVector,
    _client_side_score,
    _collection,
    _document_dict,
    _point_dict,
    _raise_for_status,
)

if TYPE_CHECKING:
    from .vector import VectorCipher

__all__ = ["AsyncClient"]


class AsyncClient:
    """An asynchronous Quiver REST client.

    Usable as an async context manager so the connection pool is closed::

        async with AsyncClient(api_key="…") as q:
            await q.create_collection("items", dim=384, metric="cosine")
            hits = await q.search("items", embedding, k=5)
    """

    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        *,
        api_key: Optional[str] = None,
        timeout: float = DEFAULT_TIMEOUT,
        verify: bool = True,
    ) -> None:
        headers = {}
        if api_key:
            headers["authorization"] = f"Bearer {api_key}"
        self._http = httpx.AsyncClient(
            base_url=base_url.rstrip("/"),
            headers=headers,
            timeout=timeout,
            verify=verify,
        )

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        """Close the underlying HTTP connection pool."""
        await self._http.aclose()

    # --- collections ---

    async def create_collection(
        self,
        name: str,
        dim: int,
        metric: str = "l2",
        *,
        index: Optional[str] = None,
        pq_subspaces: Optional[int] = None,
        filterable: Optional[Sequence[FilterableField]] = None,
        multivector: bool = False,
        vector_encryption: str = "none",
    ) -> CollectionInfo:
        """Create a collection (see :meth:`quiver.client.Client.create_collection`)."""
        body: dict[str, Any] = {"name": name, "dim": dim, "metric": metric}
        if index is not None:
            body["index"] = index
        if pq_subspaces is not None:
            body["pq_subspaces"] = pq_subspaces
        if filterable:
            body["filterable"] = [
                {"path": f.path, "field_type": f.field_type} for f in filterable
            ]
        if multivector:
            body["multivector"] = True
        if vector_encryption != "none":
            body["vector_encryption"] = vector_encryption
        return _collection((await self._send("POST", "/v1/collections", body)).json())

    async def list_collections(self) -> list[CollectionInfo]:
        """List all collections."""
        return [_collection(c) for c in (await self._send("GET", "/v1/collections")).json()]

    async def get_collection(self, name: str) -> CollectionInfo:
        """Fetch one collection's metadata."""
        return _collection((await self._send("GET", f"/v1/collections/{name}")).json())

    async def delete_collection(self, name: str) -> bool:
        """Delete a collection; returns whether it existed."""
        body = (await self._send("DELETE", f"/v1/collections/{name}")).json()
        return bool(body["existed"])

    # --- points ---

    async def upsert(self, collection: str, points: Iterable[PointInput]) -> int:
        """Insert or replace points; returns the number upserted."""
        body = {"points": [_point_dict(p) for p in points]}
        resp = await self._send("POST", f"/v1/collections/{collection}/points", body)
        return int(resp.json()["upserted"])

    async def delete_points(self, collection: str, ids: Sequence[str]) -> int:
        """Delete points by id; returns the number deleted."""
        body = {"ids": list(ids)}
        resp = await self._send("DELETE", f"/v1/collections/{collection}/points", body)
        return int(resp.json()["deleted"])

    async def get_point(self, collection: str, id: str) -> Optional[Match]:
        """Fetch a point by id, or ``None`` if it does not exist."""
        resp = await self._http.request("GET", f"/v1/collections/{collection}/points/{id}")
        if resp.status_code == 404:
            return None
        _raise_for_status(resp)
        body = resp.json()
        return Match(id=body["id"], score=0.0, payload=body.get("payload"), vector=body.get("vector"))

    async def search(
        self,
        collection: str,
        vector: Sequence[float],
        *,
        k: int = 10,
        filter: Optional[Mapping[str, Any]] = None,
        ef_search: int = 64,
        with_payload: bool = True,
        with_vector: bool = False,
    ) -> list[Match]:
        """Search for the ``k`` nearest points to ``vector`` (optionally filtered)."""
        body: dict[str, Any] = {
            "vector": list(vector),
            "k": k,
            "ef_search": ef_search,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if filter is not None:
            body["filter"] = filter
        resp = await self._send("POST", f"/v1/collections/{collection}/query", body)
        return [
            Match(id=m["id"], score=m["score"], payload=m.get("payload"), vector=m.get("vector"))
            for m in resp.json()["matches"]
        ]

    async def hybrid_search(
        self,
        collection: str,
        *,
        vector: Optional[Sequence[float]] = None,
        sparse: Optional[SparseVector] = None,
        query_text: Optional[str] = None,
        k: int = 10,
        filter: Optional[Mapping[str, Any]] = None,
        ef_search: int = 64,
        rrf_k0: float = 60.0,
        with_payload: bool = True,
        with_vector: bool = False,
    ) -> list[Match]:
        """Hybrid search fused by Reciprocal Rank Fusion (ADR-0043/0046).

        Provide a dense ``vector``, a ``sparse`` vector, and/or a full-text
        ``query_text`` (BM25); at least one is required."""
        if vector is None and sparse is None and query_text is None:
            raise ValueError(
                "hybrid_search requires a dense vector, a sparse vector, or a text query"
            )
        body: dict[str, Any] = {
            "k": k,
            "ef_search": ef_search,
            "rrf_k0": rrf_k0,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if vector is not None:
            body["vector"] = list(vector)
        if query_text is not None:
            body["query_text"] = query_text
        if sparse is not None:
            body["sparse_indices"] = [int(i) for i in sparse.indices]
            body["sparse_values"] = [float(v) for v in sparse.values]
        if filter is not None:
            body["filter"] = filter
        resp = await self._send("POST", f"/v1/collections/{collection}/query/hybrid", body)
        return [
            Match(id=m["id"], score=m["score"], payload=m.get("payload"), vector=m.get("vector"))
            for m in resp.json()["matches"]
        ]

    async def upsert_text(self, collection: str, points: Iterable[Mapping[str, Any]]) -> int:
        """Embed each point's text server-side and upsert it (ADR-0047). See
        :meth:`Client.upsert_text`."""
        body = {
            "points": [
                {"id": p["id"], "text": p["text"], **({"payload": p["payload"]} if p.get("payload") is not None else {})}
                for p in points
            ]
        }
        resp = await self._send("POST", f"/v1/collections/{collection}/points:text", body)
        return int(resp.json()["upserted"])

    async def search_text(
        self,
        collection: str,
        text: str,
        *,
        k: int = 10,
        filter: Optional[Mapping[str, Any]] = None,
        ef_search: int = 64,
        rrf_k0: float = 60.0,
        with_payload: bool = True,
        with_vector: bool = False,
        rerank: bool = False,
    ) -> list[Match]:
        """Embed ``text`` server-side and search dense ⊕ BM25, optionally reranking
        (ADR-0047). See :meth:`Client.search_text`."""
        body: dict[str, Any] = {
            "text": text,
            "k": k,
            "ef_search": ef_search,
            "rrf_k0": rrf_k0,
            "with_payload": with_payload,
            "with_vector": with_vector,
            "rerank": rerank,
        }
        if filter is not None:
            body["filter"] = filter
        resp = await self._send("POST", f"/v1/collections/{collection}/query/text", body)
        return [
            Match(id=m["id"], score=m["score"], payload=m.get("payload"), vector=m.get("vector"))
            for m in resp.json()["matches"]
        ]

    async def fetch(
        self,
        collection: str,
        *,
        filter: Optional[Mapping[str, Any]] = None,
        limit: int = 100,
        with_payload: bool = True,
        with_vector: bool = False,
    ) -> list[Match]:
        """Fetch points without ranking; an optional payload ``filter`` narrows the set."""
        body: dict[str, Any] = {
            "limit": limit,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if filter is not None:
            body["filter"] = filter
        resp = await self._send("POST", f"/v1/collections/{collection}/fetch", body)
        return [
            Match(id=p["id"], score=0.0, payload=p.get("payload"), vector=p.get("vector"))
            for p in resp.json()["points"]
        ]

    async def snapshot(self, destination: str) -> dict[str, Any]:
        """Take a consistent online snapshot of the whole database into a
        server-local ``destination`` directory (ADR-0050); admin-only. See
        :meth:`Client.snapshot`."""
        resp = await self._send("POST", "/v1/snapshot", {"destination": destination})
        return dict(resp.json())

    async def search_client_side(
        self,
        collection: str,
        query: Sequence[float],
        cipher: "VectorCipher",
        *,
        k: int = 10,
        filter: Optional[Mapping[str, Any]] = None,
        metric: str = "l2",
        candidate_limit: int = 10_000,
    ) -> list[Match]:
        """Client-side NN search over a ``client_side``-encrypted collection (ADR-0032).

        Fetches the (optionally filtered) candidate set, decrypts each vector with
        ``cipher``, ranks by ``metric``, and returns the top ``k``. The server never
        ranks and never sees the key.
        """
        q = [float(x) for x in query]
        ranked: list[tuple[float, Match]] = []
        for m in await self.fetch(
            collection, filter=filter, limit=candidate_limit, with_payload=True
        ):
            vector = cipher.open(m.payload)
            ordering, score = _client_side_score(metric, q, vector)
            ranked.append(
                (ordering, Match(id=m.id, score=score, payload=m.payload, vector=vector))
            )
        ranked.sort(key=lambda pair: pair[0])
        return [m for _, m in ranked[:k]]

    # --- documents (multi-vector / late interaction) ---

    async def upsert_documents(self, collection: str, documents: Iterable[Document]) -> int:
        """Insert or replace multi-vector documents; returns the number upserted."""
        body = {"documents": [_document_dict(d) for d in documents]}
        resp = await self._send("POST", f"/v1/collections/{collection}/documents", body)
        return int(resp.json()["upserted"])

    async def delete_documents(self, collection: str, ids: Sequence[str]) -> int:
        """Delete multi-vector documents by id; returns the number deleted."""
        body = {"ids": list(ids)}
        resp = await self._send("DELETE", f"/v1/collections/{collection}/documents", body)
        return int(resp.json()["deleted"])

    async def search_multi_vector(
        self,
        collection: str,
        query: Sequence[Sequence[float]],
        *,
        k: int = 10,
        filter: Optional[Mapping[str, Any]] = None,
        ef_search: int = 64,
        with_payload: bool = True,
        with_vector: bool = False,
    ) -> list[DocumentMatch]:
        """Rank documents by MaxSim late interaction against the ``query`` token set."""
        body: dict[str, Any] = {
            "query": [list(v) for v in query],
            "k": k,
            "ef_search": ef_search,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if filter is not None:
            body["filter"] = filter
        resp = await self._send("POST", f"/v1/collections/{collection}/documents/query", body)
        return [
            DocumentMatch(
                id=m["id"], score=m["score"], payload=m.get("payload"), vectors=m.get("vectors")
            )
            for m in resp.json()["matches"]
        ]

    # --- ergonomic helpers (RAG/agentic) ---

    async def upsert_iter(
        self,
        collection: str,
        points: Iterable[PointInput],
        *,
        batch: int = 500,
        on_progress: Optional[Callable[[int], Awaitable[None] | None]] = None,
    ) -> int:
        """Upsert a large iterable in server-friendly batches; returns the total.

        ``batch`` must stay within the server's ``max_batch_size`` (ADR-0040,
        default 1000). ``on_progress`` is called with the running total after each
        batch (may be sync or async).
        """
        total = 0
        chunk: list[PointInput] = []
        for p in points:
            chunk.append(p)
            if len(chunk) >= batch:
                total += await self.upsert(collection, chunk)
                chunk = []
                if on_progress is not None:
                    result = on_progress(total)
                    if hasattr(result, "__await__"):
                        await result  # type: ignore[func-returns-value]
        if chunk:
            total += await self.upsert(collection, chunk)
            if on_progress is not None:
                result = on_progress(total)
                if hasattr(result, "__await__"):
                    await result  # type: ignore[func-returns-value]
        return total

    async def scroll(
        self,
        collection: str,
        *,
        filter: Optional[Mapping[str, Any]] = None,
        batch: int = 500,
        with_payload: bool = True,
        with_vector: bool = False,
    ) -> AsyncIterator[Match]:
        """Yield points page by page (for export / re-embedding).

        Note: the REST ``fetch`` is limit-bounded without a server cursor, so this
        fetches up to ``batch`` points in one page. Provide a narrowing ``filter``
        for large collections; a server-side scroll cursor is a follow-up.
        """
        for m in await self.fetch(
            collection,
            filter=filter,
            limit=batch,
            with_payload=with_payload,
            with_vector=with_vector,
        ):
            yield m

    async def delete_by_filter(
        self, collection: str, filter: Mapping[str, Any], *, batch: int = 500
    ) -> int:
        """Delete every point matching ``filter``; returns the number deleted.

        Fetches matching ids (paged by ``batch``) and deletes them until none
        remain. Useful for GDPR erasure and re-indexing.
        """
        total = 0
        while True:
            ids = [m.id for m in await self.fetch(collection, filter=filter, limit=batch)]
            if not ids:
                return total
            total += await self.delete_points(collection, ids)
            if len(ids) < batch:
                return total

    # --- health ---

    async def healthz(self) -> bool:
        """Whether the server's liveness probe succeeds."""
        try:
            return (await self._http.get("/healthz")).is_success
        except httpx.HTTPError:
            return False

    # --- internals ---

    async def _send(self, method: str, path: str, json: Optional[Any] = None) -> httpx.Response:
        try:
            resp = await self._http.request(method, path, json=json)
        except httpx.HTTPError as exc:
            raise QuiverError(f"request to {path} failed: {exc}") from exc
        _raise_for_status(resp)
        return resp
