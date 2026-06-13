# ADR-0015: CI policy — manual-only workflows + local verify gate

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

The project ships complete CI/CD workflows (build, test, lint, fuzz, benchmark, security scan, release) as evidence of engineering competence. However, for this repository we deliberately do not want workflows consuming Actions minutes or sending automated email on every push/PR. We still need an authoritative, reproducible quality gate.

## Decision

- **All** GitHub Actions workflows are triggered by **`on: workflow_dispatch` only** — no `push`, `pull_request`, or `schedule` triggers.
- The **authoritative gate is local**: `just verify` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the full test suite, `cargo audit`, `cargo deny check`, and a docs build. It is run before every merge.
- `CONTRIBUTING.md` documents that workflows are manual by design and that `just verify` is the gate.

## Consequences

- **+** Zero ambient CI cost/noise; the workflows still exist, are correct, and can be dispatched on demand.
- **−** Branch protection cannot require *green CI status checks* (nothing runs automatically on a PR). Protection is therefore configured as **PR-required + no-direct-push + linear-history + no-force-push/deletions**, without required status checks. Reviewers rely on the documented, reproducible local gate.
- Contributors must run `just verify` locally; the `justfile` makes this a single command, and the same steps are what the manual workflows run, so there is no drift.

## Alternatives considered

- **Standard auto-CI on push/PR** — rejected per the project brief (cost/email noise on this repo).
- **No CI at all** — rejected: no evidence of competence and no reproducible gate definition.
- **Pre-commit hooks as the only gate** — kept for fast feedback (fmt, gitleaks) but not authoritative; the full gate is too slow for every commit.
