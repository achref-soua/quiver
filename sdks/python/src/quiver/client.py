# SPDX-License-Identifier: AGPL-3.0-only
"""A small, synchronous REST client for Quiver.

The client mirrors the server's HTTP contract (``docs/api/rest-grpc.md``):
collection CRUD, point upsert/delete/get, and filtered k-NN search. Embeddings
are produced by the caller — Quiver is model-agnostic.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Iterable, Mapping, Optional, Sequence, Union

import httpx

__all__ = [
    "Client",
    "Point",
    "Match",
    "CollectionInfo",
    "FilterableField",
    "QuiverError",
]

DEFAULT_BASE_URL = "http://127.0.0.1:6333"
DEFAULT_TIMEOUT = 30.0


class QuiverError(RuntimeError):
    """An error from the Quiver server or the transport.

    ``status`` is the HTTP status code when the failure came from the server,
    or ``None`` for a transport-level error.
    """

    def __init__(self, message: str, *, status: Optional[int] = None) -> None:
        super().__init__(message)
        self.status = status


@dataclass
class Point:
    """A point to upsert: a caller-supplied id, its vector, and an optional payload."""

    id: str
    vector: Sequence[float]
    payload: Optional[Any] = None


@dataclass
class Match:
    """A search hit (or a fetched point, with ``score`` 0.0)."""

    id: str
    score: float
    payload: Optional[Any] = None
    vector: Optional[list[float]] = None


@dataclass
class Document:
    """A multi-vector (late-interaction / ColBERT) document: an id, its set of
    token vectors, and an optional payload."""

    id: str
    vectors: Sequence[Sequence[float]]
    payload: Optional[Any] = None


@dataclass
class DocumentMatch:
    """A multi-vector document hit, ranked by MaxSim late interaction."""

    id: str
    score: float
    payload: Optional[Any] = None
    vectors: Optional[list[list[float]]] = None


@dataclass
class FilterableField:
    """A payload field declared filterable for hybrid (pre-filtered) search."""

    path: str
    field_type: str = "keyword"  # "keyword" | "numeric"


@dataclass
class CollectionInfo:
    """Metadata about a collection."""

    name: str
    dim: int
    metric: str
    count: int
    index: str = "hnsw"
    pq_subspaces: Optional[int] = None
    filterable: list[FilterableField] = field(default_factory=list)
    multivector: bool = False
    encrypted_vectors: bool = False


PointInput = Union[Point, Mapping[str, Any]]


class Client:
    """A synchronous Quiver REST client.

    Usable as a context manager so the underlying connection pool is closed::

        with Client(api_key="…") as q:
            q.create_collection("items", dim=384, metric="cosine")
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
        self._http = httpx.Client(
            base_url=base_url.rstrip("/"),
            headers=headers,
            timeout=timeout,
            verify=verify,
        )

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def close(self) -> None:
        """Close the underlying HTTP connection pool."""
        self._http.close()

    # --- collections ---

    def create_collection(
        self,
        name: str,
        dim: int,
        metric: str = "l2",
        *,
        index: Optional[str] = None,
        pq_subspaces: Optional[int] = None,
        filterable: Optional[Sequence[FilterableField]] = None,
        multivector: bool = False,
        encrypted_vectors: bool = False,
    ) -> CollectionInfo:
        """Create a collection. Raises [`QuiverError`] if the name is taken.

        ``index`` picks the structure (``hnsw`` | ``vamana`` | ``disk_vamana`` |
        ``ivf``, default ``hnsw``); ``pq_subspaces`` tunes the quantized kinds.
        ``filterable`` declares payload fields to index for hybrid (pre-filtered)
        search, each a :class:`FilterableField` of ``keyword`` or ``numeric`` type.

        ``encrypted_vectors`` marks an experimental DCPE-encrypted collection
        (ADR-0031): the caller encrypts vectors client-side with
        :class:`quiver.dcpe.DcpeCipher` before upserting. It requires the ``l2``
        metric and is **not** semantically secure — see the ``quiver.dcpe`` module.
        """
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
        if encrypted_vectors:
            body["encrypted_vectors"] = True
        return _collection(self._send("POST", "/v1/collections", body).json())

    def list_collections(self) -> list[CollectionInfo]:
        """List all collections."""
        return [_collection(c) for c in self._send("GET", "/v1/collections").json()]

    def get_collection(self, name: str) -> CollectionInfo:
        """Fetch one collection's metadata."""
        return _collection(self._send("GET", f"/v1/collections/{name}").json())

    def delete_collection(self, name: str) -> bool:
        """Delete a collection; returns whether it existed."""
        return bool(self._send("DELETE", f"/v1/collections/{name}").json()["existed"])

    # --- points ---

    def upsert(self, collection: str, points: Iterable[PointInput]) -> int:
        """Insert or replace points; returns the number upserted."""
        body = {"points": [_point_dict(p) for p in points]}
        return int(
            self._send("POST", f"/v1/collections/{collection}/points", body).json()["upserted"]
        )

    def delete_points(self, collection: str, ids: Sequence[str]) -> int:
        """Delete points by id; returns the number deleted."""
        body = {"ids": list(ids)}
        return int(
            self._send("DELETE", f"/v1/collections/{collection}/points", body).json()["deleted"]
        )

    def get_point(self, collection: str, id: str) -> Optional[Match]:
        """Fetch a point by id, or ``None`` if it does not exist."""
        resp = self._http.request("GET", f"/v1/collections/{collection}/points/{id}")
        if resp.status_code == 404:
            return None
        _raise_for_status(resp)
        body = resp.json()
        return Match(id=body["id"], score=0.0, payload=body.get("payload"), vector=body.get("vector"))

    def search(
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
        """Search for the ``k`` nearest points to ``vector``.

        ``filter`` is a Quiver filter expression (see the API docs), applied to
        payloads. Returns matches ordered nearest-first.
        """
        body: dict[str, Any] = {
            "vector": list(vector),
            "k": k,
            "ef_search": ef_search,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if filter is not None:
            body["filter"] = filter
        matches = self._send("POST", f"/v1/collections/{collection}/query", body).json()["matches"]
        return [
            Match(id=m["id"], score=m["score"], payload=m.get("payload"), vector=m.get("vector"))
            for m in matches
        ]

    # --- documents (multi-vector / late interaction) ---

    def upsert_documents(self, collection: str, documents: Iterable[Document]) -> int:
        """Insert or replace multi-vector documents; returns the number upserted."""
        body = {"documents": [_document_dict(d) for d in documents]}
        return int(
            self._send("POST", f"/v1/collections/{collection}/documents", body).json()["upserted"]
        )

    def delete_documents(self, collection: str, ids: Sequence[str]) -> int:
        """Delete multi-vector documents by id; returns the number deleted."""
        body = {"ids": list(ids)}
        return int(
            self._send("DELETE", f"/v1/collections/{collection}/documents", body).json()["deleted"]
        )

    def search_multi_vector(
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
        """Rank documents by MaxSim late interaction against the ``query`` token set.

        ``query`` is a set of token vectors; ``filter`` is a Quiver filter applied
        to each document's payload. Returns documents ordered most-relevant-first.
        """
        body: dict[str, Any] = {
            "query": [list(v) for v in query],
            "k": k,
            "ef_search": ef_search,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if filter is not None:
            body["filter"] = filter
        matches = self._send(
            "POST", f"/v1/collections/{collection}/documents/query", body
        ).json()["matches"]
        return [
            DocumentMatch(
                id=m["id"],
                score=m["score"],
                payload=m.get("payload"),
                vectors=m.get("vectors"),
            )
            for m in matches
        ]

    # --- health ---

    def healthz(self) -> bool:
        """Whether the server's liveness probe succeeds."""
        try:
            return self._http.get("/healthz").is_success
        except httpx.HTTPError:
            return False

    # --- internals ---

    def _send(self, method: str, path: str, json: Optional[Any] = None) -> httpx.Response:
        try:
            resp = self._http.request(method, path, json=json)
        except httpx.HTTPError as exc:
            raise QuiverError(f"request to {path} failed: {exc}") from exc
        _raise_for_status(resp)
        return resp


def _collection(body: Mapping[str, Any]) -> CollectionInfo:
    pq = body.get("pq_subspaces")
    return CollectionInfo(
        name=body["name"],
        dim=int(body["dim"]),
        metric=str(body["metric"]),
        count=int(body.get("count", 0)),
        index=str(body.get("index", "hnsw")),
        pq_subspaces=int(pq) if pq is not None else None,
        filterable=[
            FilterableField(
                path=str(f["path"]),
                field_type=str(f.get("field_type", "keyword")),
            )
            for f in body.get("filterable", [])
        ],
        multivector=bool(body.get("multivector", False)),
        encrypted_vectors=bool(body.get("encrypted_vectors", False)),
    )


def _point_dict(point: PointInput) -> dict[str, Any]:
    if isinstance(point, Point):
        out: dict[str, Any] = {"id": point.id, "vector": list(point.vector)}
        if point.payload is not None:
            out["payload"] = point.payload
        return out
    return dict(point)


def _document_dict(doc: Document) -> dict[str, Any]:
    out: dict[str, Any] = {"id": doc.id, "vectors": [list(v) for v in doc.vectors]}
    if doc.payload is not None:
        out["payload"] = doc.payload
    return out


def _raise_for_status(resp: httpx.Response) -> None:
    if resp.status_code < 400:
        return
    detail: Optional[str] = None
    try:
        body = resp.json()
        if isinstance(body, Mapping):
            detail = body.get("detail") or body.get("title")
    except ValueError:
        detail = None
    message = detail or resp.text or f"HTTP {resp.status_code}"
    raise QuiverError(message, status=resp.status_code)
