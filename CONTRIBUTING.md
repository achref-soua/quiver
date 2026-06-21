# Contributing to Quiver

Thanks for your interest. Quiver values correctness, security, and clarity over speed — contributions are held to that bar.

## Development setup

```bash
# Rust (stable) is pinned by rust-toolchain.toml; rustup installs it automatically.
cargo install just            # task runner
git clone https://github.com/achref-soua/quiver && cd quiver
just build
just verify                   # must pass before you push
```

## The quality gate

The `ci` and `security` workflows under `.github/workflows` run automatically on every pull request and on pushes to `main`/`develop` ([ADR-0015](./docs/adr/0015-ci-policy.md)); the heavier `build` workflow stays manual (`workflow_dispatch`). Run the same checks **locally** as your fast pre-commit gate:

```bash
just verify   # cargo fmt --check · clippy -D warnings · test · doc · cargo deny · cargo audit
```

Run it before every push. CI runs the same steps, so there is no drift.

## Workflow

- Branch off `develop`: `feature/<area>-<desc>`, `fix/<desc>`, `docs/<desc>`, `refactor/<desc>`, `perf/<desc>`, `chore/<desc>`.
- Keep PRs **small and single-purpose**. Rebase on `develop`; we **squash-merge** so history stays linear (one tidy commit per change).
- `main` and `develop` are protected — all changes land via PR.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), **imperative, lowercase subject** (`feat(index): add hnsw insert`), with the *why* in the body when non-trivial. A DCO `Signed-off-by` line (`git commit -s`) is appreciated.

## Code standards

- New code ships with **tests**; new public API ships with rustdoc; new config ships in `.env.example` with validation.
- No `unwrap()`/`expect()` on fallible production paths (clippy denies them outside tests — [ADR-0017](./docs/adr/0017-error-handling.md)).
- Every `unsafe` block carries a `// SAFETY:` justification and a test; SIMD kernels are guarded by differential tests against the scalar reference.
- `cargo fmt` is canonical; `clippy -D warnings` must be clean.

## Design first

Significant changes start with a design note or an [ADR](./docs/adr). Read [`docs/architecture/overview.md`](./docs/architecture/overview.md) and the current phase's Definition of Done in [`docs/roadmap.md`](./docs/roadmap.md) before starting.

## Security

Do **not** open public issues for vulnerabilities — see [`SECURITY.md`](./SECURITY.md) for private disclosure.
