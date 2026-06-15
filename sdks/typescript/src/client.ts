// SPDX-License-Identifier: AGPL-3.0-only
//! A small, dependency-free REST client for Quiver.
//
// Mirrors the server's HTTP contract (docs/api/rest-grpc.md): collection CRUD,
// point upsert/delete/get, and filtered k-NN search, plus the per-collection
// index choice (the memory-frugal `disk_vamana` path). Embeddings are produced
// by the caller — Quiver is model-agnostic. Uses the global `fetch`, so it runs
// on Node >= 20 and modern runtimes with no dependencies.

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

/** The index structure a collection is served by (ADR-0007). */
export type IndexKind = "hnsw" | "vamana" | "disk_vamana" | "ivf";

/** A distance metric. */
export type Metric = "l2" | "cosine" | "dot";

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
  encryptedVectors: boolean;
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
  /** Index structure; `disk_vamana` is the memory-frugal disk path (l2/cosine). */
  index?: IndexKind;
  /** Product-quantization subspaces for `disk_vamana` / `ivf` (must divide dim). */
  pqSubspaces?: number;
  /** Payload fields to index for hybrid (pre-filtered) search. */
  filterable?: FilterableField[];
  /** Create a multi-vector (late-interaction / ColBERT) collection. */
  multivector?: boolean;
  /**
   * Create an experimental DCPE-encrypted collection (ADR-0031): vectors are
   * encrypted client-side with property-preserving encryption before upserting.
   * Requires the `l2` metric and is not semantically secure.
   */
  encryptedVectors?: boolean;
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
    if (opts.encryptedVectors) body["encrypted_vectors"] = true;
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
    encryptedVectors: Boolean(b["encrypted_vectors"]),
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
