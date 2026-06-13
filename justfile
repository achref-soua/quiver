# Quiver developer commands. Run `just` (or `just --list`) to see them.
# The CI workflows are manual-only (ADR-0015); `just verify` is the real gate.

set shell := ["bash", "-cu"]

# List available recipes.
default:
    @just --list

# Build the whole workspace, including tests and examples.
build:
    cargo build --workspace --all-targets

# Run the test suite.
test:
    cargo test --workspace

# Run the Python SDK test suite (requires uv; HTTP is mocked, no server needed).
test-py:
    cd sdks/python && uv run --quiet pytest -q

# Format check + clippy with warnings denied.
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

# Auto-format the tree.
fmt:
    cargo fmt --all

# Build the API documentation.
doc:
    cargo doc --workspace --no-deps

# Dependency advisory scan. The ignored advisories are dev-only / compile-time
# transitives (rcgen's `time`, ratatui's `paste`); rationale is in deny.toml.
audit:
    cargo audit --ignore RUSTSEC-2026-0009 --ignore RUSTSEC-2024-0436

# License / advisory / source policy checks.
deny:
    cargo deny check

# The authoritative local quality gate (ADR-0015).
verify: lint test doc deny audit

# Run the server.
run *ARGS:
    cargo run -p quiver-cli -- serve {{ ARGS }}

# Launch the terminal cockpit.
tui:
    cargo run -p quiver-cli -- tui

# One-command demo: build, start an encrypted server, seed a collection, and
# print how to open the cockpit (Ctrl-C to stop). Requires uv, curl, openssl.
demo:
    bash scripts/demo.sh

# Run the benchmark harness against a running server (requires uv). Args pass
# through, e.g. `just bench --synthetic` or `just bench --dataset PATH`.
bench *ARGS:
    uv run --project bench python -m quiver_bench.run {{ ARGS }}

# Coverage report (HTML).
coverage:
    cargo llvm-cov --workspace --html

# Optimized release build.
release:
    cargo build --workspace --release

# Build the container image.
docker:
    docker build -f infra/docker/Dockerfile -t quiver:dev .
