# C4 — System Context (Level 1)

How Quiver sits among its users and the external systems it touches. (Rendered as a flowchart for reliable GitHub display; semantics follow the C4 model.)

```mermaid
flowchart TB
  dev["<b>Application / Developer</b><br/>builds on Quiver via SDK<br/>or the embedded library"]
  op["<b>Operator</b><br/>self-hosts &amp; administers<br/>the instance"]
  agent["<b>AI Agent</b><br/>drives Quiver through<br/>the MCP server"]

  subgraph sys["Quiver"]
    q["<b>Vector database</b><br/>store · index · search ·<br/>encrypt · observe"]
  end

  kms["<b>External KMS</b><br/>(optional)<br/>wraps/unwraps the master key"]
  prom["<b>Prometheus</b><br/>scrapes /metrics"]
  obj["<b>Object storage</b> S3/MinIO<br/>(optional)<br/>snapshots &amp; restore"]
  emb["<b>Embedding provider</b><br/>(caller's choice — outside Quiver)"]

  dev -->|"upsert / query<br/>gRPC · REST"| q
  agent -->|"create collection,<br/>upsert, query (MCP tools)"| q
  op -->|"TUI cockpit ·<br/>admin commands"| q
  dev -.->|"vectors are produced<br/>client-side"| emb

  q -->|"wrap / unwrap DEK"| kms
  prom -->|"pull metrics"| q
  q -->|"write / read snapshots"| obj
```

**Notes**

- **Embeddings are out of scope.** The developer's application turns content into vectors using whatever model it likes; Quiver stores and searches the resulting vectors. An optional embedder hook exists for convenience but is never required (ADR-0018).
- **KMS and object storage are optional.** Default deployment runs self-contained: the master key comes from the environment/file and snapshots go to the local filesystem. KMS and S3/MinIO are opt-in for production hardening.
- **Trust boundary.** Everything inside *Quiver* is one process. With client-side payload encryption enabled, payload plaintext never crosses into that boundary — the server stores and returns opaque ciphertext (see the threat model).
