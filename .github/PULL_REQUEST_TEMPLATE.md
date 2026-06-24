<!--
Thanks for contributing to Quiver! Keep PRs small and single-purpose.
Target the `develop` branch (never `main` directly).
-->

## Summary

<!-- What does this PR do, and why? Link any related issue (e.g. "Closes #123"). -->

## Type of change

- [ ] `feat` — new feature
- [ ] `fix` — bug fix
- [ ] `perf` — performance improvement
- [ ] `refactor` — code change that neither fixes a bug nor adds a feature
- [ ] `docs` — documentation only
- [ ] `test` — adding or correcting tests
- [ ] `build` / `ci` / `chore` — tooling, CI, or maintenance

## Checklist

- [ ] The PR is small and single-purpose, branched off `develop`.
- [ ] Commits follow [Conventional Commits](https://www.conventionalcommits.org/) and are imperative & scoped.
- [ ] `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass.
- [ ] `cargo test --workspace` passes; new code ships with tests.
- [ ] New endpoints/config ship with docs (and `.env`/config reference where relevant).
- [ ] No secrets, no `unwrap()`/`expect()`, no `console.log`/debug debris, no commented-out code.
- [ ] Benchmark/memory claims trace to a committed result (or are labelled reference-hardware-pending) — never fabricated.

## How was this tested?

<!-- Commands run, datasets used, or the reasoning if no test applies. -->
