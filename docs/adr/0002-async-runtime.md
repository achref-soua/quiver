# ADR-0002: Async runtime — Tokio

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

The server needs async I/O for the network surface (gRPC, REST, streaming, TLS). The engine, by contrast, is CPU-bound (distance computation, graph traversal, page I/O via mmap). We must choose a runtime without coupling the embeddable engine to it or starving network I/O with search work.

## Decision

Use **Tokio (multi-threaded runtime)** in the network/edge crates (`quiver-server`, and clients in `quiver-tui`/`quiver-mcp`). Keep the **engine crates synchronous and runtime-agnostic** (`quiver-core`, `quiver-index`, `quiver-query`, `quiver-simd`, `quiver-crypto`). CPU-bound and blocking work is dispatched off the async worker threads via `spawn_blocking` or a dedicated CPU thread-pool (e.g. `rayon`), never run inline on async tasks.

## Consequences

- **+** First-class compatibility with `tonic`, `axum`, `hyper`, and `rustls`, which are all Tokio-native and well-audited; the de-facto ecosystem standard.
- **+** Embedded users get the engine without being forced to adopt an async runtime; the search hot path stays off the async scheduler, so a long query cannot stall request acceptance.
- **−** Requires discipline at the seam: every potentially-blocking engine call from an async handler must be offloaded. This is enforced by review and by keeping the engine API synchronous (so the offload is explicit at the call site).

## Alternatives considered

- **async-std** — effectively unmaintained; ecosystem moved to Tokio.
- **smol** — lighter but a smaller ecosystem; less battle-tested under load.
- **glommio / monoio (thread-per-core, io_uring)** — compelling for a high-throughput ingestion data plane, but Linux-only and significantly more complex. Out of scope for single-node v1; revisit for the ingestion path post-v1.
