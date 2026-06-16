# Semantically secure vector search (client-side encryption)

Quiver can store your embeddings on a server that learns **nothing** about them —
no coordinates, no distances, no clustering, no geometry — and still give you
correct nearest-neighbour results. This is the **semantically secure** end of
Quiver's encrypted-search spectrum, and the honest counterpart to DCPE
([`dcpe.md`](dcpe.md)): where DCPE lets the server rank ciphertexts but leaks the
distance-comparison relation by design, this mode leaks none of it — at the cost
that **the server does not rank**. The client fetches the entitled set, decrypts
locally, and ranks.

> [!NOTE]
> This mode is opt-in and off by default. It is genuinely **IND-CPA** for vectors
> (the server holds only XChaCha20-Poly1305 ciphertext), reusing the same audited
> primitive as encryption-at-rest and payload encryption — **no new
> cryptography**. Its real cost is operational, not cryptographic: the server
> can't rank opaque ciphertext, so it suits **small/medium collections or
> server-pre-filtered subsets**, where downloading the candidate set to rank
> client-side is acceptable.

## The encrypted-search spectrum

You cannot have all three of: an untrusted server doing fast ANN ranking, zero
distance leakage, and practical performance. Pick two. Quiver offers the honest
points on that line, per collection (`vector_encryption`):

| Mode | Server sees | Server ranks? | Leakage | Best for |
|---|---|---|---|---|
| `none` (default) | plaintext vectors | yes | everything | trusted server, max speed |
| `dcpe` | ciphertext | **yes** (approx. L2) | approximate distance ordering, **by design** | untrusted server, ANN at scale, leak acceptable |
| `client_side` (this page) | ciphertext only | **no** | size, dimension, chosen cleartext fields, access patterns | small/medium or pre-filtered sets, **zero** geometry leakage |
| *(FHE — out of scope)* | ciphertext | yes | none | tiny "secure exact search" only |

## The scheme

A client-held `VectorCipher` seals a vector's raw little-endian `f32` bytes with
**XChaCha20-Poly1305** (the audited RustCrypto AEAD that already protects pages,
the WAL, and payloads) under a fresh random 192-bit nonce, with associated data
`quiver/vector/v1`. The result is a one-key envelope stored under the reserved
payload key `__quiver_vec__`:

```json
{ "__quiver_vec__": {
    "v":   1,
    "alg": "xchacha20poly1305",
    "dim": 8,
    "n":   "<base64 24-byte nonce>",
    "ct":  "<base64 ciphertext+tag>"
} }
```

On upsert, the client sends this blob in the payload **plus a zero placeholder
vector** of the collection's dimension. To the engine that is an ordinary point,
so there is **no on-disk format change and the `kill -9` crash gate is untouched
by construction** — the blob rides the existing payload heap. The server builds
**no ANN index** for the collection and **rejects a ranked query**; retrieval is a
**fetch** (an optional cleartext payload filter narrows the set, a limit bounds
it), and the client decrypts and ranks.

Because the sealed message is raw bytes — not transcendental floats like DCPE —
the round-trip is **bit-exact** and the envelope reproduces **byte-identically**
across the Rust, Python, and TypeScript implementations (a stronger interop
guarantee than DCPE's tolerance-based equivalence).

## What it hides, and what it leaks — honestly

Against an **honest-but-curious server** holding only ciphertexts, this mode hides
the **entire geometry**: every coordinate, all pairwise distances, norms,
clustering, and nearest-neighbour structure. The ciphertext is IND-CPA (fresh
random nonce per seal), so the same vector sealed twice is indistinguishable from
two unrelated vectors. The server cannot rank, cluster, or invert.

It leaks, by necessity rather than cryptographic weakness:

- the collection's **size** and declared **dimension**;
- whatever payload fields you **deliberately leave cleartext** to keep
  server-filterable (you choose the trade-off per field);
- **access patterns** — which points are fetched, how often, in what batches
  (hiding these needs ORAM, which is out of scope).

It composes with encryption-at-rest, RBAC, and payload encryption; it does not
replace them.

## The cost, stated plainly

The server does not rank. `Client.search_client_side` (Python) /
`Client.searchClientSide` (TypeScript) **fetches the candidate set and ranks it on
the client**. That is practical for small/medium collections, or when a cleartext
payload filter (an [ADR-0022](../adr/0022-secondary-indexes.md) secondary index)
narrows the set the server returns — not for an unfiltered top-k over tens of
millions of vectors. This is the inherent price of zero leakage, not a bug. If you
need the server to rank at scale and can accept the distance-ordering leak, use
[DCPE](dcpe.md) instead.

## Constraints

- **Any metric.** The server never ranks, so there is no metric restriction (the
  client ranks with `l2`, `cosine`, or `dot`); the collection's declared metric is
  advisory. (Multi-vector collections are not supported in this mode.)
- A **zero placeholder vector** costs `dim × 4` bytes per point on disk — the price
  of changing no on-disk format; negligible beside the payload blob.
- Encrypt and decrypt from a client holding the key; the server never sees it.

## Using it

Create the collection with `vector_encryption="client_side"`, then upsert sealed
vectors and search client-side. **Python:**

```python
from quiver import Client
from quiver.vector import VectorCipher        # pip install quiver-client[encryption]

cipher = VectorCipher.from_hex("…64 hex chars…")
with Client("https://…", api_key="…") as q:
    q.create_collection("vault", dim=8, metric="l2", vector_encryption="client_side")
    vec = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]
    # A zero placeholder vector + the sealed blob (and any cleartext fields).
    q.upsert("vault", [{"id": "a", "vector": [0.0] * 8,
                        "payload": {"tier": "gold", **cipher.seal(vec)}}])
    # Fetch + decrypt + rank, entirely client-side:
    hits = q.search_client_side("vault", my_query, cipher, k=10)
```

**TypeScript** (the cipher is at the `quiver-client/vector` subpath, an optional
`@stablelib/xchacha20poly1305` peer dependency):

```ts
import { Client } from "quiver-client";
import { VectorCipher } from "quiver-client/vector";

const cipher = VectorCipher.fromHex("…64 hex chars…");
const q = new Client("https://…", { apiKey: "…" });
await q.createCollection("vault", 8, { metric: "l2", vectorEncryption: "client_side" });
await q.upsert("vault", [{ id: "a", vector: new Array(8).fill(0),
                           payload: { tier: "gold", ...cipher.seal(vec) } }]);
const hits = await q.searchClientSide("vault", myQuery, cipher, { k: 10 });
```

The Rust reference `quiver_crypto::vector::VectorCipher` is available to embedders;
the MCP `fetch` tool retrieves the entitled set for an agent that holds the key.

## Key management

The vector-encryption key is the client's; Quiver never sees it. **Use a dedicated
key** — never your at-rest (`QUIVER_ENCRYPTION_KEY`) or payload key. Losing the key
makes the vectors unrecoverable. Ideally use a distinct key per collection.

## Status

Shipped in v0.11.0 ([ADR-0032](../adr/0032-client-side-vector-encryption.md)): the
`quiver_crypto::vector` reference cipher, the `vector_encryption = client_side`
collection mode enforced server-side (no index, ranked search rejected) with a
`fetch` path across REST/gRPC/MCP, native Python and TypeScript ciphers with a
bit-exact cross-language known-answer test, client-side `search` helpers, and an
end-to-end gate proof that the plaintext vectors never reach disk and the server
cannot rank them.
