# ADR-0017: Error handling

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

A database must handle failure precisely: callers need to distinguish "not found" from "permission denied" from "corruption detected," and the server must map internal errors to stable gRPC/REST codes without leaking internals. Rust offers several idioms; we need one consistent policy.

## Decision

- **Library crates** (`core`, `index`, `query`, `crypto`, `embed`, …) expose **typed errors** via `thiserror`, with a small, stable error enum per crate and a top-level `quiver_embed::Error` that callers match on.
- **Binary edges** (`quiver-cli`, server bootstrap, one-off tools) use **`anyhow`** for ergonomic context chaining.
- **No `unwrap()` / `expect()` on fallible production paths.** Clippy denies `unwrap_used` and `expect_used` outside `#[cfg(test)]`. Panics are reserved for genuine invariant violations and are documented with `// INVARIANT:` / `// SAFETY:` notes.
- A stable mapping turns engine errors into **gRPC `Status`** codes and **REST RFC-9457 `application/problem+json`** responses; messages are sanitized so internal paths/secrets never reach clients.

## Consequences

- **+** SDKs and the server can branch on error kinds reliably; user-facing errors are stable and safe.
- **+** The "no unwrap on prod paths" rule is a real correctness guard for a storage engine, enforced mechanically.
- **−** More upfront design of error enums and the error→status mapping table; some boilerplate (kept low with `thiserror` derives and `#[from]`).

## Alternatives considered

- **`anyhow` everywhere** — rejected: opaque to callers, can't match on kinds across an API boundary.
- **`Box<dyn Error>`** — rejected: erases structure, poor for a public library API.
- **A bespoke error framework** — unnecessary; `thiserror` + a mapping layer is sufficient and idiomatic.
