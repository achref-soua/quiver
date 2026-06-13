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

# Dependency advisory scan. RUSTSEC-2026-0009 (`time`) is a dev-only transitive
# of rcgen's TLS-test cert generation; rationale is documented in deny.toml.
audit:
    cargo audit --ignore RUSTSEC-2026-0009

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

# Coverage report (HTML).
coverage:
    cargo llvm-cov --workspace --html

# Optimized release build.
release:
    cargo build --workspace --release

# Build the container image.
docker:
    docker build -f infra/docker/Dockerfile -t quiver:dev .
