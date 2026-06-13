// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from "vitest";

import { Client, QuiverError } from "../src/client.js";

type Handler = (path: string, method: string, init: RequestInit | undefined) => Promise<Response>;

function mockFetch(handler: Handler): typeof fetch {
  return (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    return handler(new URL(url).pathname, init?.method ?? "GET", init);
  }) as typeof fetch;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function parseBody(init: RequestInit | undefined): Record<string, unknown> {
  return JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
}

describe("Quiver TypeScript client", () => {
  it("createCollection sends index + pq_subspaces and parses the response", async () => {
    let captured: { path: string; body: Record<string, unknown> } | undefined;
    const fetch = mockFetch(async (path, _method, init) => {
      captured = { path, body: parseBody(init) };
      return json({ name: "items", dim: 4, metric: "l2", index: "disk_vamana", pq_subspaces: 2, count: 0 });
    });
    const client = new Client("http://x", { apiKey: "k", fetch });
    const info = await client.createCollection("items", 4, {
      metric: "l2",
      index: "disk_vamana",
      pqSubspaces: 2,
    });
    expect(captured?.path).toBe("/v1/collections");
    expect(captured?.body).toEqual({
      name: "items",
      dim: 4,
      metric: "l2",
      index: "disk_vamana",
      pq_subspaces: 2,
    });
    expect(info.index).toBe("disk_vamana");
    expect(info.pqSubspaces).toBe(2);
  });

  it("defaults the index field out of the body and sends the bearer token", async () => {
    let body: Record<string, unknown> | undefined;
    let auth: string | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      body = parseBody(init);
      auth = (init?.headers as Record<string, string>)["authorization"];
      return json({ name: "c", dim: 3, metric: "l2", index: "hnsw", count: 0 });
    });
    const info = await new Client("http://x", { apiKey: "secret", fetch }).createCollection("c", 3);
    expect(body).toEqual({ name: "c", dim: 3, metric: "l2" });
    expect(auth).toBe("Bearer secret");
    expect(info.index).toBe("hnsw");
    expect(info.pqSubspaces).toBeUndefined();
  });

  it("upsert posts points (omitting absent payloads) and returns the count", async () => {
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      body = parseBody(init);
      return json({ upserted: 2 });
    });
    const n = await new Client("http://x", { fetch }).upsert("items", [
      { id: "a", vector: [1, 2], payload: { c: "red" } },
      { id: "b", vector: [3, 4] },
    ]);
    expect(n).toBe(2);
    expect(body?.["points"]).toEqual([
      { id: "a", vector: [1, 2], payload: { c: "red" } },
      { id: "b", vector: [3, 4] },
    ]);
  });

  it("search sends the filter and parses matches nearest-first", async () => {
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      body = parseBody(init);
      return json({ matches: [{ id: "a", score: 0.1, payload: { c: "red" } }] });
    });
    const hits = await new Client("http://x", { fetch }).search("items", [1, 2], {
      k: 5,
      filter: { eq: { field: "c", value: "red" } },
    });
    expect(body?.["k"]).toBe(5);
    expect(body?.["filter"]).toEqual({ eq: { field: "c", value: "red" } });
    expect(hits).toHaveLength(1);
    expect(hits[0]!.id).toBe("a");
  });

  it("getPoint returns null on 404", async () => {
    const fetch = mockFetch(async () => new Response("", { status: 404 }));
    const m = await new Client("http://x", { fetch }).getPoint("items", "nope");
    expect(m).toBeNull();
  });

  it("deleteCollection reports whether it existed", async () => {
    const fetch = mockFetch(async () => json({ existed: true }));
    expect(await new Client("http://x", { fetch }).deleteCollection("items")).toBe(true);
  });

  it("raises QuiverError carrying the status on a server error", async () => {
    const fetch = mockFetch(async () =>
      json({ detail: "vamana and ivf support l2 and cosine; use hnsw for dot" }, 400),
    );
    const client = new Client("http://x", { fetch });
    await expect(
      client.createCollection("x", 4, { index: "vamana", metric: "dot" }),
    ).rejects.toBeInstanceOf(QuiverError);
    await expect(
      client.createCollection("x", 4, { index: "vamana", metric: "dot" }),
    ).rejects.toMatchObject({ status: 400 });
  });

  it("healthz reflects the probe and swallows transport errors", async () => {
    const up = mockFetch(async () => new Response("ok", { status: 200 }));
    expect(await new Client("http://x", { fetch: up }).healthz()).toBe(true);
    const down = mockFetch(async () => {
      throw new Error("connection refused");
    });
    expect(await new Client("http://x", { fetch: down }).healthz()).toBe(false);
  });
});
