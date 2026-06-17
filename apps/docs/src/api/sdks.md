# Python & TypeScript SDKs

Both SDKs are thin, idiomatic clients over the [REST API](rest-grpc.md). They are
unpublished today — install from the repository — and a publish to PyPI/npm is a
launch-time task.

## Python

Install from [`sdks/python`](https://github.com/achref-soua/quiver/tree/main/sdks/python)
(`pip install ./sdks/python`):

```python
from quiver import Client, Point

with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("items", dim=3, metric="cosine")
    q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
    hits = q.search("items", [0.1, 0.2, 0.3], k=5)
```

**LangChain** and **LlamaIndex** adapters ship as extras
(`pip install "./sdks/python[langchain]"` / `[llamaindex]`), so any Quiver index —
including the memory-frugal disk path — backs a retriever, with metadata filters
mapped onto Quiver's hybrid pre-filter.

## TypeScript

Install from [`sdks/typescript`](https://github.com/achref-soua/quiver/tree/main/sdks/typescript)
(`pnpm add ./sdks/typescript`), dependency-free over the global `fetch`:

```ts
import { Client } from "quiver-client";

const q = new Client("http://127.0.0.1:6333", { apiKey: "…" });
await q.createCollection("items", 3, { metric: "cosine", index: "disk_vamana", pqSubspaces: 1 });
await q.upsert("items", [{ id: "a", vector: [0.1, 0.2, 0.3], payload: { tag: "x" } }]);
const hits = await q.search("items", [0.1, 0.2, 0.3], { k: 5 });
```

## Client-side encryption helpers

The SDKs carry the client-side ciphers as **optional subpath modules**, so the core
client stays dependency-free; install the audited crypto peer dependency only to use
them. Each has a Rust reference and a cross-language known-answer test.

| Helper | Purpose | Python | TypeScript |
|---|---|---|---|
| `PayloadCipher` | seal payload fields ([ADR-0012](https://github.com/achref-soua/quiver/blob/main/docs/adr/0012-client-side-encryption.md)) | `quiver.encryption` | `quiver-client/encryption` |
| `VectorCipher` | [opaque vectors](../security/client-side-vectors.md) (IND-CPA) | `quiver.vector` | `quiver-client/vector` |
| `DcpeCipher` | [DCPE](../security/dcpe.md) encrypted search (experimental) | `quiver.dcpe` | `quiver-client/dcpe` |

DCPE example (encrypt vectors before upsert, queries before search, with the same
cipher):

```python
from quiver import Client
from quiver.dcpe import DcpeCipher          # pip install quiver-client[dcpe]

cipher = DcpeCipher.from_hex("…64 hex chars…", approximation_factor=0.02)
with Client("https://…", api_key="…") as q:
    q.create_collection("vault", dim=8, metric="l2", vector_encryption="dcpe")
    sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8])
    q.upsert("vault", [{"id": "a", "vector": sealed.ciphertext}])
    hits = q.search("vault", cipher.encrypt_query(my_query), k=10)
```

```ts
import { DcpeCipher } from "quiver-client/dcpe"; // pnpm add @stablelib/{chacha,hkdf,hmac,sha256}

const cipher = DcpeCipher.fromHex("…64 hex chars…", 0.02);
const sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
// upsert sealed.ciphertext; search with cipher.encryptQuery(myQuery).
```
