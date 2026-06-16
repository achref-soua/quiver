# SPDX-License-Identifier: AGPL-3.0-only
"""A small, synchronous REST client for Quiver.

The client mirrors the server's HTTP contract (``docs/api/rest-grpc.md``):
collection CRUD, point upsert/delete/get, and filtered k-NN search. Embeddings
are produced by the caller — Quiver is model-agnostic.
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Iterable, Mapping, Optional, Sequence, Union

import httpx

if TYPE_CHECKING:
    from .vector import VectorCipher

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
    vector_encryption: str = "none"


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
        vector_encryption: str = "none",
    ) -> CollectionInfo:
        """Create a collection. Raises [`QuiverError`] if the name is taken.

        ``index`` picks the structure (``hnsw`` | ``vamana`` | ``disk_vamana`` |
        ``ivf``, default ``hnsw``); ``pq_subspaces`` tunes the quantized kinds.
        ``filterable`` declares payload fields to index for hybrid (pre-filtered)
        search, each a :class:`FilterableField` of ``keyword`` or ``numeric`` type.

        ``vector_encryption`` selects client-side vector encryption (the server
        never holds the key):

        * ``"none"`` — plaintext vectors, the server ranks (the default).
        * ``"dcpe"`` — experimental property-preserving encryption (ADR-0031): the
          server ranks ciphertexts, requires the ``l2`` metric, and is **not**
          semantically secure (see :class:`quiver.dcpe.DcpeCipher`).
        * ``"client_side"`` — semantically secure opaque AEAD (ADR-0032): the
          server stores blobs it cannot read and does **not** rank, so you
          :meth:`fetch` and rank locally (see :class:`quiver.vector.VectorCipher`
          and :meth:`search_client_side`).
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
        if vector_encryption != "none":
            body["vector_encryption"] = vector_encryption
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

    def fetch(
        self,
        collection: str,
        *,
        filter: Optional[Mapping[str, Any]] = None,
        limit: int = 100,
        with_payload: bool = True,
        with_vector: bool = False,
    ) -> list[Match]:
        """Fetch points without ranking; an optional payload ``filter`` narrows the
        set and ``limit`` bounds it.

        This is the retrieval path for ``client_side``-encrypted collections
        (ADR-0032): the server returns the entitled set — each payload carries the
        sealed vector under ``__quiver_vec__`` — and you decrypt and rank locally
        (see :meth:`search_client_side`). It is also a general list-points call for
        any collection. Returned matches carry ``score`` 0.0 (no ranking).
        """
        body: dict[str, Any] = {
            "limit": limit,
            "with_payload": with_payload,
            "with_vector": with_vector,
        }
        if filter is not None:
            body["filter"] = filter
        points = self._send("POST", f"/v1/collections/{collection}/fetch", body).json()[
            "points"
        ]
        return [
            Match(id=p["id"], score=0.0, payload=p.get("payload"), vector=p.get("vector"))
            for p in points
        ]

    def search_client_side(
        self,
        collection: str,
        query: Sequence[float],
        cipher: VectorCipher,
        *,
        k: int = 10,
        filter: Optional[Mapping[str, Any]] = None,
        metric: str = "l2",
        candidate_limit: int = 10_000,
    ) -> list[Match]:
        """Nearest-neighbour search over a ``client_side``-encrypted collection
        (ADR-0032), done entirely client-side.

        :meth:`fetch` es the (optionally filtered) candidate set, decrypts each
        vector with ``cipher`` (a :class:`quiver.vector.VectorCipher`), ranks by
        ``metric`` (``"l2"`` | ``"cosine"`` | ``"dot"``), and returns the top ``k``.
        The server never ranks and never sees the key. ``candidate_limit`` bounds
        how many points are fetched before ranking — this mode suits small/medium
        or pre-filtered collections.

        Each returned :class:`Match` carries the decrypted ``vector`` and a ``score``
        under the chosen metric (the raw distance for ``l2``, the similarity for
        ``cosine``/``dot``).
        """
        q = [float(x) for x in query]
        ranked: list[tuple[float, Match]] = []
        for m in self.fetch(
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
        vector_encryption=str(body.get("vector_encryption", "none")),
    )


def _client_side_score(
    metric: str, query: Sequence[float], vector: Sequence[float]
) -> tuple[float, float]:
    """Score a candidate for client-side ranking (ADR-0032).

    Returns ``(ordering, score)``: ``ordering`` sorts ascending so the nearest
    point comes first; ``score`` is the value reported on the :class:`Match`.
    """
    if metric == "l2":
        d = math.fsum((x - y) ** 2 for x, y in zip(query, vector))
        return d, d
    dot = math.fsum(x * y for x, y in zip(query, vector))
    if metric == "dot":
        return -dot, dot
    if metric == "cosine":
        nq = math.sqrt(math.fsum(x * x for x in query)) or 1.0
        nv = math.sqrt(math.fsum(y * y for y in vector)) or 1.0
        sim = dot / (nq * nv)
        return -sim, sim
    raise ValueError(f"unknown metric: {metric!r}")


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
