# quiver-client

The Python client for [Quiver](https://github.com/achref-soua/quiver), the security-first vector database. Embeddings are produced by the caller — Quiver is model-agnostic.

## Install

```bash
uv add quiver-client        # or: pip install quiver-client
```

## Usage

```python
from quiver import Client, Point

# Connect (use https:// and the api_key your server requires).
with Client("http://127.0.0.1:6333", api_key="your-api-key") as q:
    q.create_collection("items", dim=3, metric="cosine")

    q.upsert("items", [
        Point("a", [0.1, 0.2, 0.3], {"color": "red"}),
        Point("b", [0.9, 0.1, 0.0], {"color": "blue"}),
    ])

    hits = q.search(
        "items",
        [0.1, 0.2, 0.25],
        k=5,
        filter={"eq": {"field": "color", "value": "red"}},
    )
    for hit in hits:
        print(hit.id, hit.score, hit.payload)
```

`create_collection` also takes `index` (`hnsw` | `vamana` | `disk_vamana` | `ivf`)
and `pq_subspaces` to select the memory-frugal disk-resident path.

For late-interaction (ColBERT) retrieval, create a collection with
`multivector=True`, index documents as token sets with `upsert_documents`, and
rank them by MaxSim with `search_multi_vector`:

```python
q.create_collection("papers", dim=128, metric="cosine", multivector=True)
q.upsert_documents("papers", [Document("p1", token_vectors, {"title": "…"})])
hits = q.search_multi_vector("papers", query_token_vectors, k=10)
```

## Client-side payload encryption

Seal payload fields with a key Quiver never sees (install
`quiver-client[encryption]`). The server stores and returns ciphertext it
cannot read; keep fields server-filterable by leaving them in cleartext:

```python
from quiver import Client, Point
from quiver.encryption import PayloadCipher

cipher = PayloadCipher.from_hex("…64 hex chars…")   # a dedicated key, never the at-rest one
with Client("http://127.0.0.1:6333", api_key="…") as q:
    payload = {"tier": "gold", **cipher.seal({"ssn": "078-05-1120"})}  # tier stays filterable
    q.upsert("people", [Point("p1", [0.1, 0.2, 0.3], payload)])
    hit = q.get("people", "p1")
    secret = cipher.open(hit.payload)               # -> {"ssn": "078-05-1120"}
```

The envelope (XChaCha20-Poly1305) matches the Rust reference and the TypeScript
SDK byte-for-byte — see [client-side encryption](https://github.com/achref-soua/quiver/blob/main/docs/security/crypto.md#client-side-payload-encryption-adr-0012).

## Encrypted vector search (DCPE, experimental)

Encrypt the **vectors** themselves so the server can run nearest-neighbour
search without ever seeing the plaintext embeddings (install
`quiver-client[dcpe]`). This is property-preserving (distance-comparison-
preserving) encryption — **experimental, L2-only, and not semantically secure**:
it leaks the approximate distance-comparison relation by design. Use a dedicated
key, and encrypt both the data and the queries with the same cipher.

```python
from quiver import Client
from quiver.dcpe import DcpeCipher

cipher = DcpeCipher.from_hex("…64 hex chars…", approximation_factor=0.02)
with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("vault", dim=8, metric="l2", encrypted_vectors=True)
    sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8])
    q.upsert("vault", [{"id": "a", "vector": sealed.ciphertext}])
    hits = q.search("vault", cipher.encrypt_query(my_query), k=10)
```

See [ADR-0031](https://github.com/achref-soua/quiver/blob/main/docs/adr/0031-dcpe-vector-encryption.md)
and [docs/security/dcpe.md](https://github.com/achref-soua/quiver/blob/main/docs/security/dcpe.md).

## LangChain

A LangChain `VectorStore` adapter ships in `quiver.langchain` (install
`quiver-client[langchain]`):

```python
from quiver import Client
from quiver.langchain import QuiverVectorStore

store = QuiverVectorStore.from_texts(
    texts, embedding, client=Client(api_key="…"),
    collection="docs", index="disk_vamana", pq_subspaces=48,
)
docs = store.similarity_search("query", k=4)
```

## Development

```bash
uv sync            # create the venv and install dependencies
uv run pytest      # run the test suite (HTTP mocked with respx)
```

## License

AGPL-3.0-only.
