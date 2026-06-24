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

  it("strips trailing slashes from the base URL (no double slash in requests)", async () => {
    let capturedUrl = "";
    const fakeFetch = (async (input: RequestInfo | URL) => {
      capturedUrl = typeof input === "string" ? input : input.toString();
      return json([]);
    }) as typeof globalThis.fetch;
    const client = new Client("http://x:6333///", { fetch: fakeFetch });
    await client.listCollections();
    expect(capturedUrl).toBe("http://x:6333/v1/collections");
  });

  it("createCollection sends filterable fields and parses them back", async () => {
    let captured: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      captured = parseBody(init);
      return json({
        name: "people",
        dim: 4,
        metric: "l2",
        count: 0,
        index: "hnsw",
        filterable: [{ path: "city", field_type: "keyword" }],
      });
    });
    const client = new Client("http://x", { fetch });
    const info = await client.createCollection("people", 4, {
      filterable: [{ path: "city", fieldType: "keyword" }],
    });
    expect(captured?.["filterable"]).toEqual([{ path: "city", field_type: "keyword" }]);
    expect(info.filterable).toEqual([{ path: "city", fieldType: "keyword" }]);
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

  it("hybridSearch sends dense + sparse and the rrf constant, and parses matches", async () => {
    let path: string | undefined;
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (p, _method, init) => {
      path = p;
      body = parseBody(init);
      return json({
        matches: [
          { id: "a", score: 0.5 },
          { id: "b", score: 0.4 },
        ],
      });
    });
    const hits = await new Client("http://x", { fetch }).hybridSearch("kb", {
      vector: [1, 0, 0, 0],
      sparse: { indices: [1, 2], values: [5, 5] },
      k: 2,
      rrfK0: 60,
    });
    expect(path).toBe("/v1/collections/kb/query/hybrid");
    expect(body?.["vector"]).toEqual([1, 0, 0, 0]);
    expect(body?.["sparse_indices"]).toEqual([1, 2]);
    expect(body?.["sparse_values"]).toEqual([5, 5]);
    expect(body?.["rrf_k0"]).toBe(60);
    expect(hits.map((m) => m.id)).toEqual(["a", "b"]);
  });

  it("hybridSearch works pure-sparse and rejects an empty query", async () => {
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      body = parseBody(init);
      return json({ matches: [{ id: "b", score: 0.4 }] });
    });
    const client = new Client("http://x", { fetch });
    const hits = await client.hybridSearch("kb", {
      sparse: { indices: [1, 2], values: [1, 1] },
    });
    expect(body?.["vector"]).toBeUndefined();
    expect(body?.["sparse_indices"]).toEqual([1, 2]);
    expect(hits[0]!.id).toBe("b");
    await expect(client.hybridSearch("kb", {})).rejects.toThrow(/dense vector, a sparse vector/);
  });

  it("hybridSearch sends query_text for the BM25 full-text path", async () => {
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      body = parseBody(init);
      return json({ matches: [{ id: "cat", score: 1.2 }] });
    });
    const hits = await new Client("http://x", { fetch }).hybridSearch("docs", {
      queryText: "cats",
    });
    expect(body?.["query_text"]).toBe("cats");
    expect(body?.["vector"]).toBeUndefined();
    expect(hits[0]!.id).toBe("cat");
  });

  it("upsertText posts to points:text with text and optional payload (ADR-0047)", async () => {
    let path: string | undefined;
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (p, _method, init) => {
      path = p;
      body = parseBody(init);
      return json({ upserted: 2 });
    });
    const n = await new Client("http://x", { fetch }).upsertText("docs", [
      { id: "a", text: "hello world", payload: { src: "x" } },
      { id: "b", text: "second" },
    ]);
    expect(path).toBe("/v1/collections/docs/points:text");
    const points = body?.["points"] as Array<Record<string, unknown>>;
    expect(points[0]).toEqual({ id: "a", text: "hello world", payload: { src: "x" } });
    expect(points[1]).toEqual({ id: "b", text: "second" });
    expect(n).toBe(2);
  });

  it("searchText posts to query/text with the rerank flag (ADR-0047)", async () => {
    let path: string | undefined;
    let body: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (p, _method, init) => {
      path = p;
      body = parseBody(init);
      return json({ matches: [{ id: "fox", score: 2 }] });
    });
    const hits = await new Client("http://x", { fetch }).searchText("docs", "quick fox", {
      k: 5,
      rerank: true,
    });
    expect(path).toBe("/v1/collections/docs/query/text");
    expect(body?.["text"]).toBe("quick fox");
    expect(body?.["k"]).toBe(5);
    expect(body?.["rerank"]).toBe(true);
    expect(hits[0]!.id).toBe("fox");
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

  it("createCollection sends vector_encryption and parses it back", async () => {
    let createBody: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (path, _method, init) => {
      if (path === "/v1/collections") {
        createBody = parseBody(init);
        return json({
          name: "vault",
          dim: 8,
          metric: "l2",
          count: 0,
          vector_encryption: "client_side",
        });
      }
      return json({}, 404);
    });
    const client = new Client("http://x", { fetch });
    const info = await client.createCollection("vault", 8, {
      metric: "l2",
      vectorEncryption: "client_side",
    });
    expect(info.vectorEncryption).toBe("client_side");
    expect(createBody?.["vector_encryption"]).toBe("client_side");
  });

  it("fetch returns unranked points and searchClientSide ranks locally", async () => {
    const { VectorCipher } = await import("../src/vector.js");
    const cipher = VectorCipher.fromHex("11".repeat(32));
    const near = [0.75, 0.25, 0.0]; // exact in f32 ⇒ exact round-trip
    const far = [0.0, 1.0, 1.0];
    const fetch = mockFetch(async (path) => {
      if (path === "/v1/collections/vault/fetch") {
        return json({
          points: [
            { id: "far", payload: cipher.seal(far) },
            { id: "near", payload: cipher.seal(near) },
          ],
        });
      }
      return json({}, 404);
    });
    const client = new Client("http://x", { fetch });
    const points = await client.fetch("vault", { limit: 10 });
    expect(points.map((p) => p.id)).toEqual(["far", "near"]);
    expect(points.every((p) => p.score === 0)).toBe(true);
    const hits = await client.searchClientSide("vault", [1.0, 0.0, 0.0], cipher, { k: 1 });
    expect(hits.length).toBe(1);
    expect(hits[0]?.id).toBe("near");
    expect(hits[0]?.vector).toEqual(near);
  });

  it("multivector documents: create, upsert, search, and delete", async () => {
    let createBody: Record<string, unknown> | undefined;
    let upsertBody: Record<string, unknown> | undefined;
    let searchBody: Record<string, unknown> | undefined;
    let deleteCalled = false;
    const fetch = mockFetch(async (path, method, init) => {
      if (path === "/v1/collections") {
        createBody = parseBody(init);
        return json({ name: "docs", dim: 3, metric: "cosine", count: 0, multivector: true });
      }
      if (path === "/v1/collections/docs/documents" && method === "POST") {
        upsertBody = parseBody(init);
        return json({ upserted: 2 });
      }
      if (path === "/v1/collections/docs/documents" && method === "DELETE") {
        deleteCalled = true;
        return json({ deleted: 1 });
      }
      if (path === "/v1/collections/docs/documents/query") {
        searchBody = parseBody(init);
        return json({ matches: [{ id: "b", score: 1, payload: { lang: "fr" } }] });
      }
      return json({}, 404);
    });
    const client = new Client("http://x", { fetch });

    const info = await client.createCollection("docs", 3, { metric: "cosine", multivector: true });
    expect(info.multivector).toBe(true);
    expect(createBody?.["multivector"]).toBe(true);

    const n = await client.upsertDocuments("docs", [
      {
        id: "a",
        vectors: [
          [1, 0, 0],
          [0, 1, 0],
        ],
        payload: { lang: "en" },
      },
      { id: "b", vectors: [[0, 0, 1]] },
    ]);
    expect(n).toBe(2);
    expect((upsertBody?.["documents"] as unknown[]).length).toBe(2);

    const matches = await client.searchMultiVector("docs", [[0, 0, 1]], { k: 2 });
    expect(matches).toEqual([{ id: "b", score: 1, payload: { lang: "fr" } }]);
    expect(searchBody?.["query"]).toEqual([[0, 0, 1]]);

    expect(await client.deleteDocuments("docs", ["b"])).toBe(1);
    expect(deleteCalled).toBe(true);
  });

  it("createCollection sends the colbert index for a multivector collection", async () => {
    let captured: Record<string, unknown> | undefined;
    const fetch = mockFetch(async (_path, _method, init) => {
      captured = parseBody(init);
      return json({ name: "docs", dim: 3, metric: "cosine", count: 0, index: "colbert", multivector: true });
    });
    const client = new Client("http://x", { fetch });
    const info = await client.createCollection("docs", 3, {
      metric: "cosine",
      multivector: true,
      index: "colbert",
    });
    expect(captured?.["index"]).toBe("colbert");
    expect(captured?.["multivector"]).toBe(true);
    expect(info.index).toBe("colbert");
    expect(info.multivector).toBe(true);
  });

  it("snapshot posts the destination and parses the info", async () => {
    let captured: Record<string, unknown> | undefined;
    let capturedPath: string | undefined;
    const fetch = mockFetch(async (path, _method, init) => {
      capturedPath = path;
      captured = parseBody(init);
      return json({ manifest_version: 3, files: 12, bytes: 4096 });
    });
    const info = await new Client("http://x", { fetch }).snapshot("/backups/snap1");
    expect(capturedPath).toBe("/v1/snapshot");
    expect(captured?.["destination"]).toBe("/backups/snap1");
    expect(info).toEqual({ manifestVersion: 3, files: 12, bytes: 4096 });
  });

  it("upsertIter batches a large iterable and reports progress", async () => {
    const batches: number[] = [];
    const fetch = mockFetch(async (_path, _method, init) => {
      const n = (parseBody(init)["points"] as unknown[]).length;
      batches.push(n);
      return json({ upserted: n });
    });
    const client = new Client("http://x", { apiKey: "k", fetch });
    const points = Array.from({ length: 7 }, (_, i) => ({ id: `p${i}`, vector: [i] }));
    const progress: number[] = [];
    const total = await client.upsertIter("c", points, {
      batch: 3,
      onProgress: (t) => {
        progress.push(t);
      },
    });
    expect(total).toBe(7);
    expect(batches).toEqual([3, 3, 1]); // server-friendly batches, remainder flushed
    expect(progress).toEqual([3, 6, 7]); // running total after each batch
  });

  it("upsertIter accepts an async iterable", async () => {
    const fetch = mockFetch(async (_path, _method, init) => {
      const n = (parseBody(init)["points"] as unknown[]).length;
      return json({ upserted: n });
    });
    async function* gen() {
      for (let i = 0; i < 4; i++) yield { id: `p${i}`, vector: [i] };
    }
    const total = await new Client("http://x", { fetch }).upsertIter("c", gen(), { batch: 2 });
    expect(total).toBe(4);
  });

  it("scroll yields points from one fetch page", async () => {
    let capturedPath: string | undefined;
    const fetch = mockFetch(async (path, _method, init) => {
      capturedPath = path;
      expect(parseBody(init)["limit"]).toBe(2);
      return json({ points: [{ id: "a" }, { id: "b" }] });
    });
    const client = new Client("http://x", { fetch });
    const ids: string[] = [];
    for await (const m of client.scroll("c", { batch: 2 })) ids.push(m.id);
    expect(capturedPath).toBe("/v1/collections/c/fetch");
    expect(ids).toEqual(["a", "b"]);
  });

  it("deleteByFilter pages through matches until none remain", async () => {
    const pages = [
      { points: [{ id: "a" }, { id: "b" }] }, // full page -> keep going
      { points: [{ id: "c" }] }, // short page -> last
    ];
    const deleted: string[][] = [];
    let fetchCall = 0;
    const fetch = mockFetch(async (path, method, init) => {
      if (method === "POST" && path.endsWith("/fetch")) {
        return json(pages[fetchCall++] ?? { points: [] });
      }
      const ids = parseBody(init)["ids"] as string[]; // DELETE points
      deleted.push(ids);
      return json({ deleted: ids.length });
    });
    const total = await new Client("http://x", { fetch }).deleteByFilter(
      "c",
      { eq: { field: "k", value: "v" } },
      { batch: 2 },
    );
    expect(total).toBe(3);
    expect(deleted).toEqual([["a", "b"], ["c"]]);
  });
});
