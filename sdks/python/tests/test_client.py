# SPDX-License-Identifier: AGPL-3.0-only
"""Unit tests for the Quiver client, mocking the HTTP layer with respx."""

import json

import httpx
import pytest
import respx

from quiver import (
    Client,
    CollectionInfo,
    Document,
    DocumentMatch,
    FilterableField,
    Match,
    Point,
    QuiverError,
)

BASE = "http://quiver.test"


@respx.mock
def test_create_collection_sends_body_and_auth_header():
    route = respx.post(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(
            200, json={"name": "items", "dim": 4, "metric": "l2", "count": 0}
        )
    )
    with Client(BASE, api_key="secret") as q:
        info = q.create_collection("items", 4, metric="l2")
    assert info == CollectionInfo(name="items", dim=4, metric="l2", count=0)
    request = route.calls.last.request
    assert request.headers["authorization"] == "Bearer secret"
    assert json.loads(request.content) == {"name": "items", "dim": 4, "metric": "l2"}


@respx.mock
def test_create_collection_with_filterable_fields():
    route = respx.post(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(
            200,
            json={
                "name": "people",
                "dim": 4,
                "metric": "l2",
                "count": 0,
                "filterable": [{"path": "city", "field_type": "keyword"}],
            },
        )
    )
    with Client(BASE) as q:
        info = q.create_collection(
            "people", 4, filterable=[FilterableField("city", "keyword")]
        )
    assert info.filterable == [FilterableField("city", "keyword")]
    body = json.loads(route.calls.last.request.content)
    assert body["filterable"] == [{"path": "city", "field_type": "keyword"}]


@respx.mock
def test_create_collection_sends_vector_encryption():
    route = respx.post(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(
            200,
            json={
                "name": "vault",
                "dim": 8,
                "metric": "l2",
                "count": 0,
                "vector_encryption": "client_side",
            },
        )
    )
    with Client(BASE) as q:
        info = q.create_collection("vault", 8, vector_encryption="client_side")
    assert info.vector_encryption == "client_side"
    body = json.loads(route.calls.last.request.content)
    assert body["vector_encryption"] == "client_side"


@respx.mock
def test_create_collection_sends_colbert_index():
    route = respx.post(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(
            200,
            json={
                "name": "docs",
                "dim": 3,
                "metric": "cosine",
                "count": 0,
                "index": "colbert",
                "multivector": True,
            },
        )
    )
    with Client(BASE) as q:
        info = q.create_collection(
            "docs", 3, metric="cosine", multivector=True, index="colbert"
        )
    assert info.index == "colbert"
    assert info.multivector is True
    body = json.loads(route.calls.last.request.content)
    assert body["index"] == "colbert"
    assert body["multivector"] is True


@respx.mock
def test_fetch_returns_unranked_points():
    respx.post(f"{BASE}/v1/collections/vault/fetch").mock(
        return_value=httpx.Response(
            200,
            json={"points": [
                {"id": "a", "payload": {"k": 1}},
                {"id": "b", "payload": {"k": 2}},
            ]},
        )
    )
    with Client(BASE) as q:
        points = q.fetch("vault", limit=10)
    assert [p.id for p in points] == ["a", "b"]
    assert all(p.score == 0.0 for p in points)


@respx.mock
def test_search_client_side_fetches_decrypts_and_ranks():
    from quiver.vector import VectorCipher

    cipher = VectorCipher.from_hex("11" * 32)
    target = [1.0, 0.0, 0.0]
    near = [0.75, 0.25, 0.0]  # exact in f32, so the round-trip is exact
    far = [0.0, 1.0, 1.0]
    points = [
        {"id": "far", "payload": cipher.seal(far)},
        {"id": "near", "payload": cipher.seal(near)},
    ]
    respx.post(f"{BASE}/v1/collections/vault/fetch").mock(
        return_value=httpx.Response(200, json={"points": points})
    )
    with Client(BASE) as q:
        hits = q.search_client_side("vault", target, cipher, k=1)
    assert len(hits) == 1
    assert hits[0].id == "near"
    assert hits[0].vector == near


@respx.mock
def test_list_get_and_delete_collection():
    respx.get(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(
            200, json=[{"name": "a", "dim": 4, "metric": "cosine", "count": 3}]
        )
    )
    respx.get(f"{BASE}/v1/collections/a").mock(
        return_value=httpx.Response(
            200, json={"name": "a", "dim": 4, "metric": "cosine", "count": 3}
        )
    )
    respx.delete(f"{BASE}/v1/collections/a").mock(
        return_value=httpx.Response(200, json={"existed": True})
    )
    with Client(BASE) as q:
        cols = q.list_collections()
        assert cols == [CollectionInfo("a", 4, "cosine", 3)]
        assert q.get_collection("a").count == 3
        assert q.delete_collection("a") is True


@respx.mock
def test_upsert_accepts_points_and_dicts():
    route = respx.post(f"{BASE}/v1/collections/items/points").mock(
        return_value=httpx.Response(200, json={"upserted": 2})
    )
    with Client(BASE) as q:
        upserted = q.upsert(
            "items",
            [Point("a", [1.0, 2.0], {"k": 1}), {"id": "b", "vector": [3.0, 4.0]}],
        )
    assert upserted == 2
    body = json.loads(route.calls.last.request.content)
    assert body["points"][0] == {"id": "a", "vector": [1.0, 2.0], "payload": {"k": 1}}
    assert body["points"][1] == {"id": "b", "vector": [3.0, 4.0]}


@respx.mock
def test_delete_points():
    respx.delete(f"{BASE}/v1/collections/items/points").mock(
        return_value=httpx.Response(200, json={"deleted": 2})
    )
    with Client(BASE) as q:
        assert q.delete_points("items", ["x", "y"]) == 2


@respx.mock
def test_get_point_found_and_missing():
    respx.get(f"{BASE}/v1/collections/items/points/a").mock(
        return_value=httpx.Response(200, json={"id": "a", "payload": {"k": 1}, "vector": [1.0]})
    )
    respx.get(f"{BASE}/v1/collections/items/points/missing").mock(
        return_value=httpx.Response(404)
    )
    with Client(BASE) as q:
        assert q.get_point("items", "a") == Match("a", 0.0, {"k": 1}, [1.0])
        assert q.get_point("items", "missing") is None


@respx.mock
def test_search_parses_matches_and_forwards_filter():
    route = respx.post(f"{BASE}/v1/collections/items/query").mock(
        return_value=httpx.Response(
            200, json={"matches": [{"id": "a", "score": 0.1, "payload": {"c": "red"}}]}
        )
    )
    with Client(BASE) as q:
        hits = q.search(
            "items",
            [0.0, 1.0],
            k=3,
            filter={"eq": {"field": "c", "value": "red"}},
        )
    assert hits == [Match("a", 0.1, {"c": "red"}, None)]
    body = json.loads(route.calls.last.request.content)
    assert body["k"] == 3
    assert body["filter"] == {"eq": {"field": "c", "value": "red"}}
    assert body["with_payload"] is True


@respx.mock
def test_server_error_raises_quivererror_with_detail():
    respx.get(f"{BASE}/v1/collections/nope").mock(
        return_value=httpx.Response(
            404, json={"title": "Not Found", "detail": "collection nope", "status": 404}
        )
    )
    with Client(BASE) as q:
        with pytest.raises(QuiverError) as excinfo:
            q.get_collection("nope")
    assert excinfo.value.status == 404
    assert "collection nope" in str(excinfo.value)


@respx.mock
def test_healthz():
    respx.get(f"{BASE}/healthz").mock(return_value=httpx.Response(200, text="ok"))
    with Client(BASE) as q:
        assert q.healthz() is True


@respx.mock
def test_multivector_documents_roundtrip():
    create = respx.post(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(
            200,
            json={
                "name": "docs",
                "dim": 3,
                "metric": "cosine",
                "count": 0,
                "multivector": True,
            },
        )
    )
    upsert = respx.post(f"{BASE}/v1/collections/docs/documents").mock(
        return_value=httpx.Response(200, json={"upserted": 2})
    )
    search = respx.post(f"{BASE}/v1/collections/docs/documents/query").mock(
        return_value=httpx.Response(
            200, json={"matches": [{"id": "b", "score": 1.0, "payload": {"lang": "fr"}}]}
        )
    )
    delete = respx.delete(f"{BASE}/v1/collections/docs/documents").mock(
        return_value=httpx.Response(200, json={"deleted": 1})
    )
    with Client(BASE, api_key="k") as q:
        info = q.create_collection("docs", 3, metric="cosine", multivector=True)
        assert info.multivector is True
        n = q.upsert_documents(
            "docs",
            [
                Document("a", [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], {"lang": "en"}),
                Document("b", [[0.0, 0.0, 1.0]], {"lang": "fr"}),
            ],
        )
        assert n == 2
        matches = q.search_multi_vector("docs", [[0.0, 0.0, 1.0]], k=2)
        assert matches == [DocumentMatch(id="b", score=1.0, payload={"lang": "fr"})]
        assert q.delete_documents("docs", ["b"]) == 1

    assert json.loads(create.calls.last.request.content)["multivector"] is True
    up_body = json.loads(upsert.calls.last.request.content)
    assert up_body["documents"][0]["vectors"] == [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
    assert json.loads(search.calls.last.request.content)["query"] == [[0.0, 0.0, 1.0]]
    assert delete.calls.call_count == 1


@respx.mock
def test_upsert_iter_batches_and_reports_progress():
    sizes: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        n = len(json.loads(request.content)["points"])
        sizes.append(n)
        return httpx.Response(200, json={"upserted": n})

    respx.post(f"{BASE}/v1/collections/items/points").mock(side_effect=handler)
    progress: list[int] = []
    with Client(BASE) as q:
        pts = [{"id": str(i), "vector": [float(i)]} for i in range(5)]
        total = q.upsert_iter("items", pts, batch=2, on_progress=progress.append)
    assert total == 5
    assert sizes == [2, 2, 1]
    assert progress == [2, 4, 5]


@respx.mock
def test_delete_by_filter_pages_until_empty():
    fetch = respx.post(f"{BASE}/v1/collections/items/fetch").mock(
        side_effect=[
            httpx.Response(200, json={"points": [{"id": "a"}, {"id": "b"}]}),
            httpx.Response(200, json={"points": [{"id": "c"}]}),
        ]
    )
    delete = respx.delete(f"{BASE}/v1/collections/items/points").mock(
        side_effect=[
            httpx.Response(200, json={"deleted": 2}),
            httpx.Response(200, json={"deleted": 1}),
        ]
    )
    with Client(BASE) as q:
        total = q.delete_by_filter("items", {"eq": {"field": "t", "value": 1}}, batch=2)
    assert total == 3
    assert fetch.call_count == 2
    assert delete.call_count == 2
