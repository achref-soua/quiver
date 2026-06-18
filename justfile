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

# Run the TypeScript SDK suite: typecheck + tests (requires pnpm; fetch is mocked).
test-ts:
    cd sdks/typescript && pnpm install --silent && pnpm typecheck && pnpm test

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

# Build the documentation site (requires mdbook: `cargo install mdbook`).
docs:
    mdbook build apps/docs

# Regenerate the cockpit PNG screenshots into docs/assets/cockpit/ (dev-only tool,
# its own workspace so its image deps stay out of the gate). Needs a monospace TTF;
# override the default DejaVu Sans Mono with QUIVER_SHOTS_FONT.
tui-shots:
    cargo run --release --manifest-path tools/cockpit-shots/Cargo.toml

# Dependency advisory scan — no suppressions; the tree is advisory-clean.
audit:
    cargo audit

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

# Fuzz a parser target with cargo-fuzz (requires a nightly toolchain +
# cargo-fuzz; see docs/security/fuzzing.md). Targets: filter_json, page_decode,
# wal_decode. e.g. `just fuzz filter_json` or `just fuzz page_decode 300`.
fuzz target="filter_json" secs="60":
    cargo +nightly fuzz run {{ target }} -- -max_total_time={{ secs }}

# Coverage report (HTML).
coverage:
    cargo llvm-cov --workspace --html

# Optimized release build.
release:
    cargo build --workspace --release

# Build the container image.
docker:
    docker build -f infra/docker/Dockerfile -t quiver:dev .
