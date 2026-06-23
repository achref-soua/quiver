# RAG quickstart example

An end-to-end Retrieval-Augmented Generation loop against Quiver —
**chunk → embed → upsert → filtered search → assemble LLM context** — in one
dependency-light script.

It uses a tiny *deterministic* hash embedder so it runs with **no API key and no
model download**. Swap the `embed()` function for a real model
(`sentence-transformers`, OpenAI, Cohere, …) for production — Quiver is
model-agnostic and just stores the vectors you give it.

## Run

```bash
# 1. Start a local server (insecure mode is dev-only)
QUIVER_INSECURE=true QUIVER_API_KEYS=dev cargo run --release -p quiverdb-cli -- serve &

# 2. Install the SDK and run the script
pip install ./sdks/python
python examples/rag/quickstart.py
```

Override the target with `QUIVER_URL` / `QUIVER_API_KEY` if your server is
elsewhere.

## What it shows

- A collection with **filterable** `topic` (keyword) and `year` (numeric) fields.
- Batched upsert via `upsert_iter` with a progress callback.
- A **metadata-pre-filtered** k-NN search (`topic = security AND year ≥ 2026`).
- Assembling the retrieved text into grounding context for an LLM.

See the [RAG guide](../../apps/docs/src/guides/rag.md),
[tuning guide](../../apps/docs/src/guides/tuning.md), and
[agentic patterns](../../apps/docs/src/guides/agentic.md) for the full story.
