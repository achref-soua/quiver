# FAQ

### What is Quiver?

A **security-first, memory-frugal vector database** in Rust: it stores embeddings
and answers approximate-nearest-neighbour (ANN) queries, with encryption-at-rest
on by default, a clean REST + gRPC API, an MCP server for AI agents, and a retro
terminal cockpit. It runs comfortably on a laptop.

### How does it compare to Qdrant / FAISS / pgvector / LanceDB?

Quiver benchmarks every system on the **same machine** with an `ann-benchmarks`-style
harness and reports speed **at a fixed recall bar**. On SIFT1M it has measured as
second only to FAISS at recall@10 ≥ 0.95, and it matches FAISS on GIST recall. The
full methodology and raw numbers are in the README and the field guide; numbers
that need dedicated reference hardware are marked pending rather than guessed.

### Is my data encrypted?

Yes — **encryption-at-rest is on by default**, sealing every durable byte (segments,
manifest, and the write-ahead log) with XChaCha20-Poly1305 under per-collection
keys. Dropping a collection **crypto-shreds** it. Payloads can additionally be
**client-side encrypted** so the server never sees them, and two opt-in modes
address encrypting the *vectors* themselves (with honestly documented trade-offs).

### Is it production-ready?

The single-node engine is mature: a `kill -9` crash gate, concurrent reads, real
durability, and a written security audit (static + OWASP ZAP + fuzzing). Clustering,
GPU acceleration, and per-shard Raft write-HA are opt-in and off by default — the
single node is unchanged at zero overhead. Check the roadmap for the current phase.

### Can it scale beyond one node?

Yes, opt-in: sharding + scatter-gather, read replicas, a coordinator with online
elastic membership and autoscaling, and per-shard Raft write high-availability. With
no cluster configured, it is a single node.

### How do I run it?

`cargo run -p quiverdb-cli -- demo` for a zero-config demo, or `quiver serve` with
`QUIVER_API_KEYS` and `QUIVER_ENCRYPTION_KEY` set. See the configuration reference
for every setting, and the Docker/Helm guides for deployment.

### What license is it under?

**AGPL-3.0-only** — self-hostable, with the network-copyleft protection appropriate
for a database you run yourself.

### Where do I get help?

Usage questions → [Discussions](https://github.com/achref-soua/quiver/discussions);
bugs/features → [Issues](https://github.com/achref-soua/quiver/issues/new/choose);
vulnerabilities → [SECURITY.md](https://github.com/achref-soua/quiver/blob/main/SECURITY.md).
