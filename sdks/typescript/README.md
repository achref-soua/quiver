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

## Client-side payload encryption

Seal payload fields with a key Quiver never sees; the server stores and returns
ciphertext it cannot read (ADR-0012). The helper lives at the
`quiver-client/encryption` subpath, so the core client stays dependency-free —
install the optional peer dependency to use it:

```bash
pnpm add @stablelib/xchacha20poly1305
```

```ts
import { Client } from "quiver-client";
import { PayloadCipher } from "quiver-client/encryption";

const cipher = PayloadCipher.fromHex("…64 hex chars…"); // a dedicated key, never the at-rest one
const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });

// Keep `tier` server-filterable; hide `ssn` from the server.
const payload = { tier: "gold", ...cipher.seal({ ssn: "078-05-1120" }) };
await q.upsert("people", [{ id: "p1", vector: embed("…"), payload }]);

const point = await q.getPoint("people", "p1");
const secret = cipher.open(point!.payload); // -> { ssn: "078-05-1120" }
```

The envelope (XChaCha20-Poly1305) matches the Rust reference and the Python SDK
byte-for-byte — see [client-side encryption](https://github.com/achref-soua/quiver/blob/main/docs/security/crypto.md#client-side-payload-encryption-adr-0012).

## Develop

```bash
pnpm install
pnpm typecheck   # tsc --noEmit
pnpm test        # vitest
pnpm build       # tsc -> dist/ (ESM + .d.ts)
```
