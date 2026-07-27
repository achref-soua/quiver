// SPDX-License-Identifier: AGPL-3.0-only
//
// TypeScript-SDK acceptance: drive the **built** SDK against a live server.
//
// The SDK's own suite mocks `fetch` entirely, so it proves the client's shape but
// never that the shape matches what the server actually sends. A field renamed
// server-side, a status code reclassified, an envelope changed — all ship green
// under mocks. This closes that: everything below crosses a real socket to a real
// encrypted server.
//
// Deliberately a smoke test of the wire contract, not a mirror of
// acceptance_sdk.py's 50 checks — the Python script already covers index kinds,
// quantization, and the encryption modes against the same server. What is unique
// here is the TypeScript client's own serialization.
//
// Run from scripts/acceptance.sh, or by hand:
//   node scripts/acceptance_sdk.mjs http://127.0.0.1:7333 <api-key>

import { Client, QuiverError } from "../sdks/typescript/dist/index.js";

const [baseUrl, apiKey] = process.argv.slice(2);
if (!baseUrl || !apiKey) {
  console.error("usage: node acceptance_sdk.mjs <base-url> <api-key>");
  process.exit(2);
}

let checks = 0;
function ok(label) {
  checks += 1;
  console.log(`  ok: ${label}`);
}
function assert(condition, label) {
  if (!condition) {
    console.error(`FAIL: ${label}`);
    process.exit(1);
  }
  ok(label);
}

const COLLECTION = "ts_acceptance";
const q = new Client(baseUrl, { apiKey });

// Points in three loose clusters so nearest-neighbour assertions are unambiguous.
const POINTS = [
  { id: "s1", vector: [0.9, 0.1, 0.0, 0.0], payload: { topic: "search", rank: 1 } },
  { id: "s2", vector: [0.8, 0.2, 0.1, 0.0], payload: { topic: "search", rank: 2 } },
  { id: "d1", vector: [0.1, 0.9, 0.0, 0.0], payload: { topic: "storage", rank: 3 } },
  { id: "d2", vector: [0.0, 0.8, 0.2, 0.0], payload: { topic: "storage", rank: 4 } },
  { id: "o1", vector: [0.0, 0.1, 0.9, 0.0], payload: { topic: "ops", rank: 5 } },
];

assert(await q.healthz(), "healthz reports the server is live");

// Start clean so a re-run is idempotent.
try {
  await q.deleteCollection(COLLECTION);
} catch {
  // absent is fine
}

const created = await q.createCollection(COLLECTION, 4, {
  metric: "cosine",
  filterable: [
    { path: "topic", fieldType: "keyword" },
    { path: "rank", fieldType: "numeric" },
  ],
});
assert(created.name === COLLECTION, "createCollection returns the collection");
assert(created.dim === 4, "the declared dimension round-trips");

assert((await q.upsert(COLLECTION, POINTS)) === POINTS.length, "upsert acknowledges every point");

const info = await q.getCollection(COLLECTION);
assert(info.count === POINTS.length, "getCollection reports the upserted count");

// The regression this file exists for: an acknowledged write must be visible to the
// next search, not silently absent while an index builds (ADR-0081).
const hits = await q.search(COLLECTION, [0.9, 0.1, 0.0, 0.0], { k: 3 });
assert(hits.length === 3, "search returns k results immediately after the write");
assert(hits[0].id === "s1", "the nearest point is the one at the query location");
assert(hits[0].payload?.topic === "search", "payloads come back on the hit");

// A pre-filtered (hybrid) search over a declared keyword field.
const filtered = await q.search(COLLECTION, [0.9, 0.1, 0.0, 0.0], {
  k: 5,
  filter: { eq: { field: "topic", value: "storage" } },
});
assert(filtered.length === 2, "the keyword filter narrows to the matching points");
assert(
  filtered.every((m) => m.payload?.topic === "storage"),
  "every filtered hit satisfies the filter",
);

const fetched = await q.getPoint(COLLECTION, "d1");
assert(fetched?.id === "d1", "getPoint round-trips a single point");
assert((await q.getPoint(COLLECTION, "nope")) === null, "an absent point is null, not an error");

assert((await q.deletePoints(COLLECTION, ["o1"])) === 1, "deletePoints acknowledges the delete");
assert((await q.getCollection(COLLECTION)).count === POINTS.length - 1, "the count reflects it");

// `scroll` is limit-bounded (one page, no server cursor) and both SDKs document
// that honestly, so ask for a page wide enough to hold the collection.
const scrolled = [];
for await (const point of q.scroll(COLLECTION, { batch: 100 })) {
  scrolled.push(point.id);
}
assert(scrolled.length === POINTS.length - 1, "scroll yields every remaining point");

// Errors must arrive as QuiverError with the server's status, not a raw fetch throw
// or a silently-swallowed failure.
let rejected = null;
try {
  await q.getCollection("does_not_exist");
} catch (e) {
  rejected = e;
}
assert(rejected instanceof QuiverError, "a missing collection raises QuiverError");
assert(rejected.status === 404, "the server's 404 survives to the client");

// An unauthenticated client must be refused by the real auth layer.
let denied = null;
try {
  await new Client(baseUrl).listCollections();
} catch (e) {
  denied = e;
}
assert(denied instanceof QuiverError, "a keyless client is rejected");
assert(denied.status === 401, "the rejection is a 401 from the server");

assert(await q.deleteCollection(COLLECTION), "deleteCollection tears it down");

console.log(`\n== TypeScript SDK acceptance PASSED: ${checks} checks ==`);
