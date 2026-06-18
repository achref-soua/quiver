# Quiver

**The security-first vector database.** Client-side-encryptable, memory-frugal
approximate-nearest-neighbour search that runs on a laptop — with a retro terminal
cockpit.

![The Quiver cockpit dashboard](https://raw.githubusercontent.com/achref-soua/quiver/main/docs/assets/cockpit/dashboard.png)

Quiver is a from-scratch, native-Rust vector database. It is not trying to
out-scale Milvus or out-feature Qdrant; its defensible edge is the **combination**
of three things, executed well:

- **Security-first, by default.** Encryption-at-rest is on out of the box, sealing
  every durable byte — segments, the manifest, *and* the write-ahead log — with
  XChaCha20-Poly1305. Payloads can be client-side-encrypted so the server never
  sees them; **vectors** can be encrypted too, either with the experimental
  distance-comparison-preserving [DCPE](features/encrypted-search.md) mode or the
  semantically secure [client-side opaque](security/client-side-vectors.md) mode.
  API-key scopes, RBAC, tenant isolation, an audit log, and crypto-shredding round
  it out. Only audited cryptography (RustCrypto AEAD/KDF + `rustls`) — never a
  home-grown primitive.
- **Memory frugality.** A disk-resident graph index (DiskANN/Vamana) plus
  quantization (product / scalar / binary) serve large datasets from a laptop's RAM
  budget. The headline metric is **memory at a fixed recall**.
- **Developer experience.** A single static binary; embeddable *and* server modes;
  a `ratatui` cockpit with a 2-D constellation view of the vector space; idiomatic
  Python and TypeScript SDKs; and an MCP server so AI agents can drive it.

We say plainly what Quiver does **not** do: billion-scale needs a server (a laptop
comfortably serves tens-to-hundreds of millions); there is no homomorphic search in
core; and each encryption mode states its exact trust boundary. See the honest
[threat model](security/threat-model.md).

> *The name.* A quiver holds arrows, and an arrow is a vector — apt for a database
> of them. And in mathematics a *quiver* is a directed graph, which is exactly what
> an HNSW or Vamana index is. The cockpit wears that identity in amber phosphor.

## Where to start

- New to Quiver? Read the [Concepts](concepts.md), then run the
  [Quickstart](quickstart.md).
- Running it yourself? See [Self-hosting & configuration](self-hosting.md).
- Building against it? See the [REST & gRPC](api/rest-grpc.md) reference, the
  [MCP server](api/mcp.md), and the [SDKs](api/sdks.md).
- Curious how it works? Start with the [architecture deep dive](architecture/deep-dive.md)
  and the [decision records](architecture/adrs.md).

Quiver is open source under the **AGPL-3.0** license. The source lives at
[github.com/achref-soua/quiver](https://github.com/achref-soua/quiver).
