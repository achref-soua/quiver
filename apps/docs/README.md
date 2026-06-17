# Quiver documentation site

The Quiver docs site, built with [mdBook](https://rust-lang.github.io/mdBook/)
(pure Rust, no Node toolchain — [ADR-0035](../../docs/adr/0035-docs-site-and-dcpe-hardening.md)).

## Build

```bash
cargo install mdbook        # once
just docs                   # build to apps/docs/book/  (or: mdbook build apps/docs)
mdbook serve apps/docs      # live-reload preview on http://localhost:3000
```

## Structure

The connective chapters (introduction, concepts, quickstart, self-hosting, the
feature overviews, SDK usage) are authored in `src/`. The deep reference chapters
**reuse the canonical docs** in the repository's top-level `docs/` tree via mdBook's
`{{#include}}` preprocessor — the security docs (`crypto`, `dcpe`,
`client-side-vectors`, `threat-model`), the architecture/index design, and the
migration, replication, REST/gRPC, and MCP references — so there is a single source
of truth and the site never drifts from the repo docs.

> Note: a few intra-document cross-links inside the embedded reference chapters
> point at the repository layout rather than the rendered site; the connective
> chapters and the [ADR index](src/architecture/adrs.md) link out explicitly. Tidying
> those embedded links is a launch-polish follow-up.

The built site (`book/`) is git-ignored; publish it from CI or a static host.
