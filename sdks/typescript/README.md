# quiver-client (TypeScript)

A small, dependency-free TypeScript/JavaScript client for
[Quiver](https://github.com/achref-soua/quiver) — a security-first, memory-frugal
vector database. It mirrors the server's REST contract and uses the global
`fetch`, so it runs on Node ≥ 20 and modern runtimes with no dependencies.

```bash
pnpm add quiver-client
```

```ts
import { Client } from "quiver-client";

const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });

// Create a memory-frugal, disk-resident collection (PQ codes in RAM,
// graph + vectors on encrypted SSD), or use "hnsw" (default) / "ivf".
await q.createCollection("items", 384, {
  metric: "cosine",
  index: "disk_vamana",
  pqSubspaces: 48,
});

await q.upsert("items", [
  { id: "a", vector: embed("…"), payload: { tag: "x" } },
]);

const hits = await q.search("items", embed("query"), {
  k: 5,
  filter: { eq: { field: "tag", value: "x" } },
});
```

Embeddings are produced by the caller — Quiver is model-agnostic.

## API

| Method | Description |
|---|---|
| `createCollection(name, dim, { metric?, index?, pqSubspaces? })` | Create a collection and pick its index |
| `listCollections()` / `getCollection(name)` / `deleteCollection(name)` | Collection CRUD |
| `upsert(collection, points)` / `deletePoints(collection, ids)` / `getPoint(collection, id)` | Points |
| `search(collection, vector, { k?, filter?, efSearch?, withPayload?, withVector? })` | Filtered k-NN |
| `healthz()` | Liveness probe |

Errors from the server (or transport) reject with a `QuiverError` carrying the
HTTP `status`. A custom `fetch` can be injected via the constructor for testing
or a bespoke transport.

## Develop

```bash
pnpm install
pnpm typecheck   # tsc --noEmit
pnpm test        # vitest
pnpm build       # tsc -> dist/ (ESM + .d.ts)
```
