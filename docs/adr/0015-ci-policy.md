# ADR-0015: CI policy — automatic PR checks + local verify gate

- **Status:** Accepted (amended 2026-06-22)
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

The project ships complete CI/CD workflows (build, test, lint, fuzz, benchmark, security scan, release) as evidence of engineering competence. Originally — while the account's Actions were billing-constrained — we deliberately ran nothing automatically and relied solely on a local gate. The repository is now public (Actions minutes are free for public repositories), so running the gates automatically on every PR costs nothing and gives reviewers real status checks. We still want one authoritative, reproducible gate that does not drift from CI.

## Decision

- The **correctness gates run automatically**: `ci` (fmt · clippy · test · doc) and `security` (cargo-deny · cargo-audit · gitleaks) trigger on **`pull_request`** and on **`push` to `main`/`develop`**, with `workflow_dispatch` kept as a manual fallback.
- The **heavy `build` workflow** (release binary + Docker image build/scan) stays **`workflow_dispatch`-only** — it is an artifact check, not a correctness gate, and is too slow to run per PR.
- **`release`** is **tag-triggered** (`v*.*.*`) plus `workflow_dispatch` (ADR-0044).
- The **local `just verify`** (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the full test suite, `cargo audit`, `cargo deny check`, docs build) remains the **fast pre-commit gate** and runs the *same* steps as CI, so the two never drift.

## Consequences

- **+** Every PR gets real, green/red status checks; reviewers no longer have to take the local gate on faith. Zero cost on the public repo.
- **+** Branch protection *can* now require green CI status checks (the `ci` and `security` jobs) in addition to PR-required + no-direct-push + linear-history + no-force-push/deletions. Enabling required checks is a follow-up ruleset change.
- **−** Some redundancy between the local gate and CI — accepted, because the local gate is the fast feedback loop and CI is the enforced source of truth.
- Concurrency groups cancel superseded in-progress runs so a busy PR does not pile up jobs.

## Alternatives considered

- **Standard auto-CI on push/PR** — rejected per the project brief (cost/email noise on this repo).
- **No CI at all** — rejected: no evidence of competence and no reproducible gate definition.
- **Pre-commit hooks as the only gate** — kept for fast feedback (fmt, gitleaks) but not authoritative; the full gate is too slow for every commit.
