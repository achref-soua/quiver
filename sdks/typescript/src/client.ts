// SPDX-License-Identifier: AGPL-3.0-only
//! A small, dependency-free REST client for Quiver.
//
// Mirrors the server's HTTP contract (docs/api/rest-grpc.md): collection CRUD,
// point upsert/delete/get, and filtered k-NN search, plus the per-collection
// index choice (the memory-frugal `disk_vamana` path). Embeddings are produced
// by the caller — Quiver is model-agnostic. Uses the global `fetch`, so it runs
// on Node >= 20 and modern runtimes with no dependencies.

// Type-only import (erased at compile time, so no runtime dependency is added)
// for the client-side search helper; the cipher itself lives at the
// `quiver-client/vector` subpath to keep this core client dependency-free.
import type { VectorCipher } from "./vector.js";

const DEFAULT_BASE_URL = "http://127.0.0.1:6333";
const DEFAULT_TIMEOUT_MS = 30_000;

/** An error from the Quiver server or the transport. `status` is the HTTP code
 * when the failure came from the server, or `undefined` for a transport error. */
export class QuiverError extends Error {
  readonly status?: number;
  constructor(message: string, status?: number) {
    super(message);
    this.name = "QuiverError";
    this.status = status;
  }
}

/** A point to upsert: a caller-supplied id, its vector, and an optional payload. */
export interface Point {
  id: string;
  vector: number[];
  payload?: unknown;
}

/** A search hit (or a fetched point, with `score` 0). */
export interface Match {
  id: string;
  score: number;
  payload?: unknown;
  vector?: number[];
}

/** A multi-vector (late-interaction / ColBERT) document: an id, its set of token
 * vectors, and an optional payload. */
export interface Document {
  id: string;
  vectors: number[][];
  payload?: unknown;
}

/** A multi-vector document hit, ranked by MaxSim late interaction. */
export interface DocumentMatch {
  id: string;
  score: number;
  payload?: unknown;
  vectors?: number[][];
}

/** The index structure a collection is served by (ADR-0007, ADR-0034).
 * `colbert` is the ColBERTv2/PLAID token-pool index for multivector collections. */
export type IndexKind = "hnsw" | "vamana" | "disk_vamana" | "ivf" | "colbert";

/** A distance metric. */
export type Metric = "l2" | "cosine" | "dot";

/** Client-side vector encryption mode — the server never holds the key (ADR-0031,
 * ADR-0032): `none` (plaintext, the server ranks), `dcpe` (the server ranks
 * ciphertexts but leaks distance ordering by design; not semantically secure), or
 * `client_side` (semantically secure opaque AEAD; the server does not rank, so you
 * fetch and rank locally). */
export type VectorEncryption = "none" | "dcpe" | "client_side";

/** The value type of a filterable payload field (ADR-0022). */
export type FieldType = "keyword" | "numeric";

/** A payload field declared filterable for hybrid (pre-filtered) search. */
export interface FilterableField {
  path: string;
  fieldType?: FieldType;
}

/** Metadata about a collection. */
export interface CollectionInfo {
  name: string;
  dim: number;
  metric: string;
  count: number;
  index: IndexKind;
  pqSubspaces?: number;
  filterable: FilterableField[];
  multivector: boolean;
  vectorEncryption: VectorEncryption;
}

/** Options for constructing a {@link Client}. */
export interface ClientOptions {
  apiKey?: string;
  timeoutMs?: number;
  /** Inject a `fetch` implementation (for tests or a custom transport). */
  fetch?: typeof fetch;
}

/** Options for {@link Client.createCollection}. */
export interface CreateCollectionOptions {
  metric?: Metric;
  /** Index structure; `disk_vamana` is the memory-frugal disk path (l2/cosine);
   * `colbert` is the ColBERTv2/PLAID token-pool index for multivector collections. */
  index?: IndexKind;
  /** Product-quantization subspaces for `disk_vamana` / `ivf` (must divide dim). */
  pqSubspaces?: number;
  /** Payload fields to index for hybrid (pre-filtered) search. */
  filterable?: FilterableField[];
  /** Create a multi-vector (late-interaction / ColBERT) collection. */
  multivector?: boolean;
  /**
   * Client-side vector encryption (the server never holds the key): `"dcpe"` is
   * experimental property-preserving encryption (ADR-0031; the server ranks,
   * requires `l2`, not semantically secure); `"client_side"` is semantically
   * secure opaque AEAD (ADR-0032; the server does not rank — use
   * {@link Client.fetch} / {@link Client.searchClientSide}). Defaults to `"none"`.
   */
  vectorEncryption?: VectorEncryption;
}

/** Options for {@link Client.search}. */
export interface SearchOptions {
  k?: number;
  /** A Quiver payload filter expression (see the API docs). */
  filter?: unknown;
  efSearch?: number;
  withPayload?: boolean;
  withVector?: boolean;
}

/** A sparse query vector for hybrid search (ADR-0043): parallel dimension ids
 * (`indices`) and weights (`values`). */
export interface SparseVector {
  indices: number[];
  values: number[];
}

/** Options for {@link Client.hybridSearch}. Provide `vector`, `sparse`, or both;
 * at least one is required. */
export interface HybridSearchOptions {
  /** Dense query vector (omit for pure-sparse/text search). */
  vector?: number[];
  /** Sparse query vector (omit for pure-dense/text search). */
  sparse?: SparseVector;
  /** Full-text query, tokenized server-side and scored by BM25 (ADR-0046). */
  queryText?: string;
  k?: number;
  /** A Quiver payload filter expression (applied on both sides). */
  filter?: unknown;
  efSearch?: number;
  /** RRF rank-bias constant (default 60). */
  rrfK0?: number;
  withPayload?: boolean;
  withVector?: boolean;
}

/** Options for {@link Client.fetch}. */
export interface FetchOptions {
  /** A Quiver payload filter expression to narrow the set. */
  filter?: unknown;
  /** Maximum number of points to return (default 100). */
  limit?: number;
  withPayload?: boolean;
  withVector?: boolean;
}

/** Options for {@link Client.searchClientSide}. */
export interface ClientSideSearchOptions {
  k?: number;
  /** A Quiver payload filter expression (applied server-side, on cleartext fields). */
  filter?: unknown;
  /** Metric to rank by, client-side (default `"l2"`). */
  metric?: Metric;
  /** How many candidates to fetch before ranking locally (default 10000). */
  candidateLimit?: number;
}

/** A synchronous-feeling, promise-based Quiver REST client. */
export class Client {
  readonly #baseUrl: string;
  readonly #headers: Record<string, string>;
  readonly #timeoutMs: number;
  readonly #fetch: typeof fetch;

  constructor(baseUrl: string = DEFAULT_BASE_URL, opts: ClientOptions = {}) {
    this.#baseUrl = baseUrl.replace(/\/+$/, "");
    this.#headers = { "content-type": "application/json" };
    if (opts.apiKey) {
      this.#headers["authorization"] = `Bearer ${opts.apiKey}`;
    }
    this.#timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.#fetch = opts.fetch ?? globalThis.fetch.bind(globalThis);
  }

  // --- collections ---

  /** Create a collection. Rejects with {@link QuiverError} if the name is taken
   * or the index/metric combination is unsupported. */
  async createCollection(
    name: string,
    dim: number,
    opts: CreateCollectionOptions = {},
  ): Promise<CollectionInfo> {
    const body: Record<string, unknown> = { name, dim, metric: opts.metric ?? "l2" };
    if (opts.index) body["index"] = opts.index;
    if (opts.pqSubspaces !== undefined) body["pq_subspaces"] = opts.pqSubspaces;
    if (opts.filterable && opts.filterable.length > 0) {
      body["filterable"] = opts.filterable.map((f) => ({
        path: f.path,
        field_type: f.fieldType ?? "keyword",
      }));
    }
    if (opts.multivector) body["multivector"] = true;
    if (opts.vectorEncryption && opts.vectorEncryption !== "none") {
      body["vector_encryption"] = opts.vectorEncryption;
    }
    return toCollection(await this.#json("POST", "/v1/collections", body));
  }

  /** List all collections. */
  async listCollections(): Promise<CollectionInfo[]> {
    const body = (await this.#json("GET", "/v1/collections")) as unknown[];
    return body.map(toCollection);
  }

  /** Fetch one collection's metadata. */
  async getCollection(name: string): Promise<CollectionInfo> {
    return toCollection(await this.#json("GET", `/v1/collections/${encodeURIComponent(name)}`));
  }

  /** Delete a collection; resolves to whether it existed. */
  async deleteCollection(name: string): Promise<boolean> {
    const body = (await this.#json("DELETE", `/v1/collections/${encodeURIComponent(name)}`)) as {
      existed?: boolean;
    };
    return Boolean(body.existed);
  }

  // --- points ---

  /** Insert or replace points; resolves to the number upserted. */
  async upsert(collection: string, points: Point[]): Promise<number> {
    const body = { points: points.map(pointDict) };
    const res = (await this.#json(
      "POST",
      `/v1/collections/${encodeURIComponent(collection)}/points`,
      body,
    )) as { upserted?: number };
    return Number(res.upserted ?? 0);
  }

  /** Delete points by id; resolves to the number deleted. */
  async deletePoints(collection: string, ids: string[]): Promise<number> {
    const res = (await this.#json(
      "DELETE",
      `/v1/collections/${encodeURIComponent(collection)}/points`,
      { ids },
    )) as { deleted?: number };
    return Number(res.deleted ?? 0);
  }

  /** Fetch a point by id, or `null` if it does not exist. */
  async getPoint(collection: string, id: string): Promise<Match | null> {
    const path = `/v1/collections/${encodeURIComponent(collection)}/points/${encodeURIComponent(id)}`;
    const resp = await this.#send("GET", path);
    if (resp.status === 404) return null;
    await throwForStatus(resp);
    const body = (await resp.json()) as Match;
    return { id: body.id, score: 0, payload: body.payload, vector: body.vector };
  }

  /** Search for the `k` nearest points to `vector`, nearest first. */
  async search(collection: string, vector: number[], opts: SearchOptions = {}): Promise<Match[]> {
    const body: Record<string, unknown> = {
      vector,
      k: opts.k ?? 10,
      ef_search: opts.efSearch ?? 64,
      with_payload: opts.withPayload ?? true,
      with_vector: opts.withVector ?? false,
    };
    if (opts.filter !== undefined) body["filter"] = opts.filter;
    const res = (await this.#json(
      "POST",
      `/v1/collections/${encodeURIComponent(collection)}/query`,
      body,
    )) as { matches?: Match[] };
    return (res.matches ?? []).map((m) => ({
      id: m.id,
      score: m.score,
      payload: m.payload,
      vector: m.vector,
    }));
  }

  /** Hybrid search fused with Reciprocal Rank Fusion (ADR-0043/0046). Provide a
   * dense `vector`, a `sparse` query vector, and/or a full-text `queryText` (scored
   * by BM25) — at least one is required. The same payload `filter` applies to every
   * side. */
  async hybridSearch(collection: string, opts: HybridSearchOptions = {}): Promise<Match[]> {
    if (opts.vector === undefined && opts.sparse === undefined && opts.queryText === undefined) {
      throw new QuiverError(
        "hybridSearch requires a dense vector, a sparse vector, or a text query",
      );
    }
    const body: Record<string, unknown> = {
      k: opts.k ?? 10,
      ef_search: opts.efSearch ?? 64,
      rrf_k0: opts.rrfK0 ?? 60,
      with_payload: opts.withPayload ?? true,
      with_vector: opts.withVector ?? false,
    };
    if (opts.vector !== undefined) body["vector"] = opts.vector;
    if (opts.queryText !== undefined) body["query_text"] = opts.queryText;
    if (opts.sparse !== undefined) {
      body["sparse_indices"] = opts.sparse.indices;
      body["sparse_values"] = opts.sparse.values;
    }
    if (opts.filter !== undefined) body["filter"] = opts.filter;
    const res = (await this.#json(
      "POST",
      `/v1/collections/${encodeURIComponent(collection)}/query/hybrid`,
      body,
    )) as { matches?: Match[] };
    return (res.matches ?? []).map((m) => ({
      id: m.id,
      score: m.score,
      payload: m.payload,
      vector: m.vector,
    }));
  }

  /** Fetch points without ranking; an optional payload `filter` narrows the set
   * and `limit` bounds it. The retrieval path for `client_side`-encrypted
   * collections (ADR-0032): the server returns the entitled set (each payload
   * carries the sealed vector under `__quiver_vec__`) and you decrypt and rank
   * locally (see {@link searchClientSide}). Also a general list-points call for
   * any collection; returned matches carry `score` 0. */
  async fetch(collection: string, opts: FetchOptions = {}): Promise<Match[]> {
    const body: Record<string, unknown> = {
      limit: opts.limit ?? 100,
      with_payload: opts.withPayload ?? true,
      with_vector: opts.withVector ?? false,
    };
    if (opts.filter !== undefined) body["filter"] = opts.filter;
    const res = (await this.#json(
      "POST",
      `/v1/collections/${encodeURIComponent(collection)}/fetch`,
      body,
    )) as { points?: Match[] };
    return (res.points ?? []).map((p) => ({
      id: p.id,
      score: 0,
      payload: p.payload,
      vector: p.vector,
    }));
  }

  /** Nearest-neighbour search over a `client_side`-encrypted collection (ADR-0032),
   * done entirely client-side: {@link fetch} the (optionally filtered) candidate
   * set, decrypt each vector with `cipher` (a `VectorCipher` from
   * `quiver-client/vector`), rank by metric, and return the top `k`. The server
   * never ranks and never sees the key. This mode suits small/medium or
   * pre-filtered collections; `candidateLimit` bounds how many points are fetched
   * before ranking. Each result carries the decrypted `vector` and a `score` under
   * the chosen metric. */
  async searchClientSide(
    collection: string,
    query: number[],
    cipher: VectorCipher,
    opts: ClientSideSearchOptions = {},
  ): Promise<Match[]> {
    const metric = opts.metric ?? "l2";
    const points = await this.fetch(collection, {
      filter: opts.filter,
      limit: opts.candidateLimit ?? 10000,
      withPayload: true,
    });
    const ranked = points.map((m) => {
      const vector = cipher.open(m.payload);
      const [ordering, score] = clientSideScore(metric, query, vector);
      return { ordering, match: { id: m.id, score, payload: m.payload, vector } };
    });
    ranked.sort((a, b) => a.ordering - b.ordering);
    return ranked.slice(0, opts.k ?? 10).map((r) => r.match);
  }

  // --- documents (multi-vector / late interaction) ---

  /** Insert or replace multi-vector documents; resolves to the number upserted. */
  async upsertDocuments(collection: string, documents: Document[]): Promise<number> {
    const body = { documents: documents.map(documentDict) };
    const res = (await this.#json(
      "POST",
      `/v1/collections/${encodeURIComponent(collection)}/documents`,
      body,
    )) as { upserted?: number };
    return Number(res.upserted ?? 0);
  }

  /** Delete multi-vector documents by id; resolves to the number deleted. */
  async deleteDocuments(collection: string, ids: string[]): Promise<number> {
    const res = (await this.#json(
      "DELETE",
      `/v1/collections/${encodeURIComponent(collection)}/documents`,
      { ids },
    )) as { deleted?: number };
    return Number(res.deleted ?? 0);
  }

  /** Rank documents by MaxSim late interaction against the `query` token set. */
  async searchMultiVector(
    collection: string,
    query: number[][],
    opts: SearchOptions = {},
  ): Promise<DocumentMatch[]> {
    const body: Record<string, unknown> = {
      query,
      k: opts.k ?? 10,
      ef_search: opts.efSearch ?? 64,
      with_payload: opts.withPayload ?? true,
      with_vector: opts.withVector ?? false,
    };
    if (opts.filter !== undefined) body["filter"] = opts.filter;
    const res = (await this.#json(
      "POST",
      `/v1/collections/${encodeURIComponent(collection)}/documents/query`,
      body,
    )) as { matches?: DocumentMatch[] };
    return (res.matches ?? []).map((m) => ({
      id: m.id,
      score: m.score,
      payload: m.payload,
      vectors: m.vectors,
    }));
  }

  // --- health ---

  /** Whether the server's liveness probe succeeds. */
  async healthz(): Promise<boolean> {
    try {
      const resp = await this.#send("GET", "/healthz");
      return resp.ok;
    } catch {
      return false;
    }
  }

  // --- internals ---

  async #send(method: string, path: string, body?: unknown): Promise<Response> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.#timeoutMs);
    try {
      return await this.#fetch(`${this.#baseUrl}${path}`, {
        method,
        headers: this.#headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
      });
    } catch (err) {
      throw new QuiverError(`request to ${path} failed: ${String(err)}`);
    } finally {
      clearTimeout(timer);
    }
  }

  async #json(method: string, path: string, body?: unknown): Promise<unknown> {
    const resp = await this.#send(method, path, body);
    await throwForStatus(resp);
    return resp.json();
  }
}

function clientSideScore(
  metric: Metric,
  query: number[],
  vector: number[],
): [number, number] {
  if (metric === "l2") {
    let d = 0;
    for (let i = 0; i < query.length; i++) {
      const diff = (query[i] ?? 0) - (vector[i] ?? 0);
      d += diff * diff;
    }
    return [d, d];
  }
  let dot = 0;
  for (let i = 0; i < query.length; i++) dot += (query[i] ?? 0) * (vector[i] ?? 0);
  if (metric === "dot") return [-dot, dot];
  let nq = 0;
  let nv = 0;
  for (let i = 0; i < query.length; i++) nq += (query[i] ?? 0) ** 2;
  for (let i = 0; i < vector.length; i++) nv += (vector[i] ?? 0) ** 2;
  const sim = dot / ((Math.sqrt(nq) || 1) * (Math.sqrt(nv) || 1));
  return [-sim, sim];
}

function toCollection(body: unknown): CollectionInfo {
  const b = body as Record<string, unknown>;
  return {
    name: String(b["name"]),
    dim: Number(b["dim"]),
    metric: String(b["metric"]),
    count: Number(b["count"] ?? 0),
    index: (b["index"] as IndexKind) ?? "hnsw",
    pqSubspaces: b["pq_subspaces"] === undefined ? undefined : Number(b["pq_subspaces"]),
    filterable: Array.isArray(b["filterable"])
      ? (b["filterable"] as Record<string, unknown>[]).map((f) => ({
          path: String(f["path"]),
          fieldType: (f["field_type"] as FieldType) ?? "keyword",
        }))
      : [],
    multivector: Boolean(b["multivector"]),
    vectorEncryption:
      typeof b["vector_encryption"] === "string"
        ? (b["vector_encryption"] as VectorEncryption)
        : "none",
  };
}

function documentDict(doc: Document): Record<string, unknown> {
  const out: Record<string, unknown> = { id: doc.id, vectors: doc.vectors };
  if (doc.payload !== undefined) out["payload"] = doc.payload;
  return out;
}

function pointDict(point: Point): Record<string, unknown> {
  const out: Record<string, unknown> = { id: point.id, vector: point.vector };
  if (point.payload !== undefined && point.payload !== null) out["payload"] = point.payload;
  return out;
}

async function throwForStatus(resp: Response): Promise<void> {
  if (resp.status < 400) return;
  let detail: string | undefined;
  try {
    const body = (await resp.clone().json()) as Record<string, unknown>;
    detail = (body["detail"] as string) ?? (body["title"] as string);
  } catch {
    detail = undefined;
  }
  const fallback = (await resp.text().catch(() => "")) || `HTTP ${resp.status}`;
  throw new QuiverError(detail ?? fallback, resp.status);
}
