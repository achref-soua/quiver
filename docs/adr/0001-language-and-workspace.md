# ADR-0001: Language and workspace layout

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Quiver must be memory-frugal, fast on the distance hot path (no GC pauses mid-search), shippable as a single static binary, usable both embedded and as a server, and security-first. It builds its own storage engine, index structures, distance kernels, and on-disk format. We need a language and a code organization that make that tractable and testable, and that a senior systems reviewer would respect.

## Decision

Implement Quiver in **Rust (stable channel)** as a **Cargo workspace** of focused crates (see [`../architecture/overview.md`](../architecture/overview.md)): `quiver-simd`, `quiver-crypto`, `quiver-core`, `quiver-index`, `quiver-query`, `quiver-proto`, `quiver-embed`, `quiver-server`, `quiver-tui`, `quiver-mcp`, `quiver-cli`.

- Build the **core from scratch** (storage, WAL, on-disk format, indexes, kernels, query planner, wire protocol).
- Use a **minimal set of vetted crates** only where reinventing is reckless: async runtime, TLS, crypto, serialization, TUI, CLI parsing.
- **No embedded database engine** (no RocksDB/LMDB/sqlite) — the storage engine is ours.
- Crate boundaries enforce an **acyclic dependency DAG**; domain logic stays in the lower crates, framework code at the edges.

## Consequences

- **+** Memory control (mmap, explicit layouts), predictable latency (no GC), memory safety, first-class SIMD via `core::arch`, a single static binary, and a strong async/TLS/gRPC ecosystem.
- **+** The engine (`core`/`index`/`query`) is runtime- and framework-free, so it is unit-testable in isolation and reusable in embedded mode.
- **−** Steeper contributor ramp and longer compile times (mitigated by workspace incremental builds and `sccache` in CI). `unsafe` must be justified with `// SAFETY:` notes and tests (enforced by review + clippy).
- **MSRV policy:** track the latest stable Rust; document the Minimum Supported Rust Version; pin the toolchain with `rust-toolchain.toml` when the Cargo workspace is scaffolded (Phase 1).

## Alternatives considered

- **C++** — comparable performance but weaker safety and tooling, harder dependency story for a security-first project.
- **Go** — excellent ergonomics but GC pauses on the search path and weaker low-level memory/SIMD control.
- **Zig** — attractive but an immature ecosystem for TLS/gRPC/crypto that we must not hand-roll.
- **Single monolithic crate** — rejected: poor separation of concerns, weaker testability, no embedded/server seam.
