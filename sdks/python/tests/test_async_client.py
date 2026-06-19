# SPDX-License-Identifier: AGPL-3.0-only
"""Unit tests for the async Quiver client, mocking HTTP with respx.

Coroutines are driven with ``asyncio.run`` so no pytest-asyncio plugin is needed;
respx patches the httpx transport for the AsyncClient just as for the sync one.
"""

import asyncio
import json

import httpx
import pytest
import respx

from quiver import AsyncClient, CollectionInfo, Match

BASE = "http://quiver.test"


@respx.mock
def test_async_create_collection_sends_body_and_auth():
    route = respx.post(f"{BASE}/v1/collections").mock(
        return_value=httpx.Response(200, json={"name": "items", "dim": 4, "metric": "l2", "count": 0})
    )

    async def run():
        async with AsyncClient(BASE, api_key="secret") as q:
            return await q.create_collection("items", 4, metric="l2")

    info = asyncio.run(run())
    assert info == CollectionInfo(name="items", dim=4, metric="l2", count=0)
    assert route.calls.last.request.headers["authorization"] == "Bearer secret"
    assert json.loads(route.calls.last.request.content) == {"name": "items", "dim": 4, "metric": "l2"}


@respx.mock
def test_async_search_parses_matches():
    respx.post(f"{BASE}/v1/collections/items/query").mock(
        return_value=httpx.Response(
            200, json={"matches": [{"id": "a", "score": 0.5, "payload": {"t": 1}}]}
        )
    )

    async def run():
        async with AsyncClient(BASE) as q:
            return await q.search("items", [0.1, 0.2], k=3)

    hits = asyncio.run(run())
    assert hits == [Match(id="a", score=0.5, payload={"t": 1}, vector=None)]


@respx.mock
def test_async_upsert_iter_batches():
    calls: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        calls.append(len(body["points"]))
        return httpx.Response(200, json={"upserted": len(body["points"])})

    respx.post(f"{BASE}/v1/collections/items/points").mock(side_effect=handler)
    progressed: list[int] = []

    async def run():
        async with AsyncClient(BASE) as q:
            pts = [{"id": str(i), "vector": [float(i)]} for i in range(7)]
            return await q.upsert_iter("items", pts, batch=3, on_progress=progressed.append)

    total = asyncio.run(run())
    assert total == 7
    assert calls == [3, 3, 1]  # three batches
    assert progressed == [3, 6, 7]


@respx.mock
def test_async_delete_by_filter_pages_until_empty():
    pages = [
        {"points": [{"id": "a"}, {"id": "b"}]},  # full page (batch=2) -> continue
        {"points": [{"id": "c"}]},               # short page -> stop after delete
    ]
    fetch = respx.post(f"{BASE}/v1/collections/items/fetch").mock(
        side_effect=[httpx.Response(200, json=p) for p in pages]
    )
    deleted_counts = [2, 1]
    delete = respx.delete(f"{BASE}/v1/collections/items/points").mock(
        side_effect=[httpx.Response(200, json={"deleted": n}) for n in deleted_counts]
    )

    async def run():
        async with AsyncClient(BASE) as q:
            return await q.delete_by_filter("items", {"eq": {"field": "t", "value": 1}}, batch=2)

    total = asyncio.run(run())
    assert total == 3
    assert fetch.call_count == 2
    assert delete.call_count == 2


def test_async_client_exported():
    # Smoke: the symbol is importable from the package root.
    from quiver import AsyncClient as AC

    assert AC is AsyncClient
