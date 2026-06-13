# Repo Scaffold Plan (the bridge to Phase 1)

This is the exact shape the repository takes when Phase 1 begins. It is a *plan*, not code — no Cargo project is created during Phase 0.

## Directory tree

```text
quiver/
├─ crates/
│  ├─ quiver-simd/        # SIMD distance kernels + feature detection
│  ├─ quiver-crypto/      # audited-crypto wrappers (NO custom primitives)
│  ├─ quiver-core/        # storage engine, WAL, on-disk format, pages
│  ├─ quiver-index/       # HNSW, Vamana, IVF, quantization
│  ├─ quiver-query/       # planner, hybrid filtered search
│  ├─ quiver-proto/       # gRPC (.proto) + REST DTOs + OpenAPI
│  ├─ quiver-embed/       # embeddable library API
│  ├─ quiver-server/      # daemon: axum + tonic, auth, RBAC, audit
│  ├─ quiver-tui/         # ratatui cockpit (API client)
│  ├─ quiver-mcp/         # MCP server
│  └─ quiver-cli/         # single binary entrypoint (serve|tui|mcp|admin|bench)
├─ sdks/
│  ├─ python/             # uv-managed SDK (+ LangChain/LlamaIndex adapters)
│  └─ typescript/         # pnpm SDK
├─ bench/                 # ann-benchmarks-style harness + dataset fetcher (datasets git-ignored)
├─ docs/                  # design docs, ADRs, C4, threat model, runbooks (this package)
├─ infra/docker/          # multi-stage Dockerfiles (distroless, non-root)
├─ .github/workflows/     # manual-only (workflow_dispatch) CI
├─ .scratch/              # git-ignored working notes & spikes (never committed)
├─ Cargo.toml             # workspace manifest
├─ rust-toolchain.toml    # pin stable channel + clippy/rustfmt
├─ justfile               # one-liners for everything
├─ deny.toml              # cargo-deny config
├─ README.md  CONTRIBUTING.md  SECURITY.md  LICENSE
```

## Workspace manifest (sketch)

`Cargo.toml` at the root: `resolver = "3"`, `members = ["crates/*"]`, a `[workspace.dependencies]` table pinning shared crate versions once, and a shared `[workspace.lints]` block applied by every crate:

```toml
[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_docs = "warn"            # public crates promote to deny

[workspace.lints.clippy]
unwrap_used = "deny"             # see ADR-0017 (test code exempt)
expect_used = "deny"
todo = "warn"
dbg_macro = "deny"
```

## Crate dependency sketch (internal + key external)

| Crate | Internal deps | Key external deps |
|---|---|---|
| `quiver-simd` | — | `cfg-if` (dispatch); `std::arch` |
| `quiver-crypto` | — | `ring` / RustCrypto AEADs, `rustls`, `zeroize`, `hkdf` |
| `quiver-core` | `quiver-crypto` | `memmap2`, `crc32c`, `bytes`, `thiserror` |
| `quiver-index` | `quiver-core`, `quiver-simd` | `rand`, `ordered-float` |
| `quiver-query` | `quiver-index`, `quiver-core` | `roaring` (bitmaps for filters) |
| `quiver-proto` | — | `tonic`, `prost`, `serde`, `utoipa` (OpenAPI) |
| `quiver-embed` | `quiver-query`, `quiver-core`, `quiver-crypto` | `thiserror` |
| `quiver-server` | `quiver-embed`, `quiver-proto`, `quiver-crypto` | `axum`, `tonic`, `tokio`, `tower`, `tracing`, `figment` |
| `quiver-tui` | `quiver-proto` | `ratatui`, `crossterm` |
| `quiver-mcp` | `quiver-proto` (or `quiver-embed`) | MCP SDK (`rmcp`) |
| `quiver-cli` | `quiver-server`, `quiver-tui`, `quiver-mcp`, `quiver-embed` | `clap` |

Exact versions are pinned in `[workspace.dependencies]` at scaffold time against the then-current stable releases (verified during Phase 1, not guessed now).

## `justfile` targets (planned)

`build`, `test`, `lint` (fmt --check + clippy -D warnings), `verify` (lint + test + audit + deny + doc — the gate), `run` (serve), `tui`, `mcp`, `bench`, `fuzz`, `demo` (seed an encrypted demo collection), `docker`, `cast` (regenerate the TUI asciinema), `audit`, `deny`, `coverage` (llvm-cov).

## CI workflow set (all `on: workflow_dispatch` — ADR-0015)

- `ci.yml` — fmt, clippy `-D warnings`, test, `cargo deny`, `cargo audit`, doc build.
- `build.yml` — release build (Linux x86_64/aarch64), Docker build, image scan (Trivy), Syft SBOM.
- `fuzz.yml` — `cargo fuzz` targets for a bounded duration (wire-protocol + on-disk parsers).
- `bench.yml` — the ann-benchmarks harness on a self-hosted/manual runner; uploads CSVs.
- `security.yml` — CodeQL + Semgrep (where supported for Rust) + `cargo deny`.
- `release.yml` — release-please-style changelog/version, cross-built binaries, GHCR image, GitHub Release.

## Dockerfile plan (`infra/docker/`)

Multi-stage: a `cargo-chef` (or pinned cache-mount) builder stage producing a static (musl) `quiver` binary, copied into a **distroless/static** non-root final image with a `HEALTHCHECK` hitting `/healthz`, `.dockerignore` excluding `target/`, `.scratch/`, datasets. Pinned base image digests.

## First Phase-1 PRs (suggested order)

1. `chore: scaffold cargo workspace + justfile + manual CI + docker + project files` (compiles, `just verify` green on empty crates with `//! ` docs and a smoke test).
2. `feat(simd): distance kernels with feature detection + differential tests`.
3. `feat(core): storage engine — segments, WAL, recovery, checksums`.
4. `feat(index): HNSW`.
5. `feat(proto|server): gRPC + REST surface`.
6. `feat(crypto): encryption-at-rest secure-by-default`.
7. `feat(tui): metrics + collection browser`.
8. `feat(sdk-py): Python client`; `chore(bench): SIFT1M harness + numbers`; `docs: README + quickstart + demo`.
