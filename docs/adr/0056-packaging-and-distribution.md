# ADR-0056: Packaging & distribution — publish pipeline, Helm chart, CHANGELOG

- **Status:** Accepted
- **Date:** 2026-06-23
- **Deciders:** Achref Soua

## Context

Quiver installs today from source (`cargo install --path crates/quiver-cli`),
from the tag-triggered GitHub-release binaries (ADR-0044), and from the SDK
source trees (`pip install ./sdks/python`, `pnpm add ./sdks/typescript`). Three
launch gaps remain:

1. **No published packages.** The `quiver` binary is not on crates.io, and the
   SDKs are not on PyPI / npm. The crates.io name `quiver-cli` is held by an
   **unrelated third-party crate** (recorded since v0.12.0); `quiver-client` was
   free on PyPI and npm when last checked. The crates also lack the metadata
   crates.io requires (`description`, `keywords`, `categories`, `readme`).
2. **No changelog.** There is no `CHANGELOG.md`; the release history lives only
   in git tags, the roadmap, and GitHub release notes.
3. **No Kubernetes story.** `infra/` has Docker and a Grafana dashboard but no
   Helm chart or raw manifests, so self-hosting on a cluster is undocumented.

The registry tokens, the live publish, and confirming a free CLI name are
**owner actions** — this ADR wires and verifies the pipeline; it does not claim
a live publish.

## Decision

- **`CHANGELOG.md`** in [Keep a Changelog](https://keepachangelog.com/) format,
  backfilled `v0.1.0 → v0.20.1` from the roadmap and tags, with an
  `[Unreleased]` section maintained going forward and a link reference per tag.

- **Crate publish metadata.** Every workspace crate gains the metadata
  crates.io requires — `description`, `keywords`, `categories`, and
  `readme` — on top of the workspace-inherited `license`, `repository`,
  `authors`, and `rust-version`. Internal deps already carry a `version`
  alongside `path`, so they rewrite cleanly to registry deps on publish.

- **CLI crate name.** `quiver-cli` is squatted, so the CLI crate is published
  under an available name (candidate **`quiverdb`**; the *binary* stays
  `quiver`). The exact name is confirmed against the live registry at publish
  time (owner action) — the workspace path/dep name `quiver-cli` is internal and
  unaffected.

- **Publish pipeline (`release.yml`).** Three tag-gated publish jobs run after
  the binary release, each **guarded by a repository secret** so a fork or a
  token-less repo skips it cleanly rather than failing the release:
  - **crates.io** — `cargo publish` in dependency-DAG order, gated on
    `CARGO_REGISTRY_TOKEN`.
  - **PyPI** — `python -m build` + `twine upload` for `sdks/python`, gated on
    `PYPI_API_TOKEN`.
  - **npm** — `npm publish` for `sdks/typescript`, gated on `NPM_TOKEN`.

- **Publishability is CI-verified, honestly.** A `package` job on every PR runs
  `cargo package` (validates the crate metadata and that each crate builds as a
  package), `twine check` on the built Python distribution, and
  `npm publish --dry-run` / `npm pack` for the TS SDK. This keeps the packages
  publishable without contacting a registry. A full registry `cargo publish
  --dry-run` of a *dependent* crate cannot pass until its dependencies are on
  crates.io (the registry chicken-and-egg), so the real first publish is the
  owner running the DAG-ordered job once — the CI guard catches every *metadata*
  regression before then.

- **Helm chart + manifests.** `infra/helm/quiver` deploys the server
  (Deployment + Service + ConfigMap + optional Ingress + a PVC for the data
  directory + a Secret for the master key), with `helm lint` and `helm template`
  run in CI. Raw manifests under `infra/k8s/` give a Helm-free path. A
  self-hosting docs page documents both.

## Consequences

- **+** A clear path to published `cargo install quiverdb`, `pip install
  quiver-client`, `npm i quiver-client`, and `helm install quiver` — the
  remaining v1.0.0 distribution gaps are wired and guarded.
- **+** CI fails the moment a crate loses publish metadata or the chart stops
  linting, so the pipeline cannot silently rot.
- **−** The first live publish is a manual, owner-token, DAG-ordered step; the
  CI dry-runs verify metadata, not a real upload (stated plainly above).
- **−** The published CLI name diverges from the internal crate name
  (`quiverdb` vs `quiver-cli`); the binary name `quiver` is unchanged, so users
  are unaffected.

## Implementation

- `CHANGELOG.md` (Keep a Changelog, backfilled).
- `[package.metadata]` completion across `crates/*/Cargo.toml`.
- `release.yml` publish jobs (crates.io / PyPI / npm), secret-gated.
- A `package` verification job in `ci.yml`.
- `infra/helm/quiver/**` and `infra/k8s/**`, with `helm lint`/`template` in CI.
- `apps/docs/src/self-hosting/kubernetes.md` (or equivalent) for the cluster path.

## Verification

- `cargo package` succeeds for every crate; `twine check` and `npm pack` pass.
- `helm lint infra/helm/quiver` and `helm template` render without error.
- The publish jobs are present and secret-gated (no live publish asserted).
