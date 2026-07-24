# ADR-0078: Publish the documentation site to GitHub Pages

- **Status:** Accepted
- **Date:** 2026-07-24
- **Deciders:** Achref Soua

## Context

The documentation *content* already exists as a complete mdBook under `apps/docs`
([ADR-0035](0035-docs-site-and-dcpe-hardening.md)): concepts, quickstart,
self-hosting, configuration, the feature guides, the REST/gRPC + MCP + SDK
references, the security docs, a Kubernetes guide, and an architecture deep dive —
33 pages. Crucially, the deep-reference chapters `{{#include}}` the canonical
top-level `docs/` so there is **one source of truth**, not a fork. It builds clean
with `just docs` (`mdbook build apps/docs`).

But it was never *published*. There is no GitHub Pages site (`/pages` returns 404),
no deploy workflow, and the `book/` output is git-ignored. The v1.0.0 launch DoD
requires "the docs site is live" ([roadmap](../roadmap.md)); this is the remaining
gap for that item.

## Decision

**Publish the existing mdBook to GitHub Pages.** A `docs` workflow builds the book
with a pinned mdBook (0.5.3, the locally verified version) and deploys it via the
Actions-native Pages flow (`configure-pages` → `upload-pages-artifact` →
`deploy-pages`). The site is served as a **project page** at
`https://achref-soua.github.io/quiver/`.

- **Deploy from `main` only.** The live site therefore matches the *released*
  product, never unreleased `develop` — a public docs site advertising unshipped
  features would be dishonest. It activates on the next `develop → main` release
  bridge (owner-gated, like every release here). Pull requests build the book as a
  health gate but do not deploy; `workflow_dispatch` allows a manual re-publish.
- **Subpath-correct.** `book.toml` gains `site-url = "/quiver/"` so the 404 page and
  any absolute references resolve under the project-page subpath. Page navigation is
  relative, so local `mdbook serve` at `/` is unaffected.
- **Not a required check.** The workflow runs but is not wired into branch
  protection, so a transient Pages hiccup never blocks a merge.

## Consequences

- **Docs go live at the next release** — infrastructure-complete now, published on
  the next bridge to `main`. Pages must be enabled once with the "GitHub Actions"
  build source (a one-time repo setting).
- **Health gate.** Because PRs now build the book, a broken `{{#include}}` path or a
  dangling `SUMMARY.md` link fails a check instead of silently shipping a broken
  page — a gap the prior `cargo doc`-only CI step did not cover.
- **No content fork, no new runtime dependency.** The published site *is* the
  mdBook; the only additions are CI actions (pinned, matching the repo's existing
  `actions/*` / `peaceiris` convention) and one `book.toml` line.

## Alternatives considered

- **Stand up a Fumadocs/Nextra site.** Rejected. It would **fork the content** (the
  explicit anti-goal) — either duplicating the Markdown or re-authoring it — and
  drag a Node/Next.js build toolchain into a pure-Rust repo, for zero reader-facing
  gain over the mdBook that already renders the canonical docs. Laziest correct
  path: publish what exists.
- **Deploy from `develop`** so the site is live immediately. Rejected: it would
  present unreleased features as shipped. Matching the live site to `main` is the
  honest default; `workflow_dispatch` covers the rare need for an out-of-band
  preview.
- **Classic `gh-pages` branch (build artifact committed to a branch).** Rejected:
  the Actions-native Pages flow needs no artifact branch and no `book/` in history.
