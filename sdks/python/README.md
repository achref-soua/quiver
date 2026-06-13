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

## Development

```bash
uv sync            # create the venv and install dependencies
uv run pytest      # run the test suite (HTTP mocked with respx)
```

## License

AGPL-3.0-only.
