# SPDX-License-Identifier: AGPL-3.0-only
"""Unit tests for the Quiver client, mocking the HTTP layer with respx."""

import json

import httpx
import pytest
import respx

from quiver import Client, CollectionInfo, Match, Point, QuiverError

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
