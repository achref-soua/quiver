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

# Run the Go SDK suite: vet + tests (requires the Go toolchain; httptest-mocked).
test-go:
    cd sdks/go && gofmt -l . && go vet ./... && go test ./...

# Format check + clippy with warnings denied.
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

# Auto-format the tree.
fmt:
    cargo fmt --all

# Build the API documentation. `-D warnings` matches CI so a broken intra-doc
# link fails the local gate too (not just in CI).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Build the documentation site (requires mdbook: `cargo install mdbook`).
docs:
    mdbook build apps/docs

# Regenerate the cockpit PNG screenshots into docs/assets/cockpit/ (dev-only tool,
# its own workspace so its image deps stay out of the gate). Needs a monospace TTF;
# override the default DejaVu Sans Mono with QUIVER_SHOTS_FONT.
tui-shots:
    cargo run --release --manifest-path tools/cockpit-shots/Cargo.toml

# Render docs/diagrams/*.mmd to docs/assets/diagrams/*.svg (mermaid-cli on demand
# via npx). Locally, point PUPPETEER_EXECUTABLE_PATH at any Chromium.
diagrams:
    bash docs/diagrams/render.sh

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
    cargo run -p quiverdb-cli -- serve {{ ARGS }}

# Launch the terminal cockpit.
tui:
    cargo run -p quiverdb-cli -- tui

# One-command demo: build, start an encrypted server, seed a collection, and
# print how to open the cockpit (Ctrl-C to stop). Requires uv, curl, openssl.
demo:
    bash scripts/demo.sh

# Real-user acceptance run: boot an encrypted server and drive every external
# surface (REST, Python SDK across all index kinds + both encrypted modes + multi
# -vector, CLI import, MCP). Requires uv, curl, openssl. See
# docs/testing/manual-acceptance.md.
acceptance:
    bash scripts/acceptance.sh

# Run the benchmark harness against a running server (requires uv). Args pass
# through, e.g. `just bench --synthetic` or `just bench --dataset PATH`.
bench *ARGS:
    uv run --project bench python -m quiver_bench.run {{ ARGS }}

# Multi-DB comparison runner (ADR-0037). Install competitor deps first:
#   uv pip install --project bench --group competitors
# Run all competitors on the siftsmall smoke dataset:
#   just bench-compare --smoke
# Run selected competitors on SIFT1M (needs bench/datasets/sift/):
#   just bench-compare --dataset sift1m --competitors faiss,lancedb,quiver
bench-compare *ARGS:
    uv run --project bench python -m quiver_bench.comparison {{ ARGS }}

# Generate the comparison report from existing CSV results.
#   just bench-report
# Reads docs/benchmarks/results/comparison-v0.18.0/ and writes
# docs/benchmarks/results/comparison-v0.18.0/comparison-v0.18.0.md
bench-report:
    uv run --project bench python -m quiver_bench.report \
        docs/benchmarks/results/comparison-v0.18.0

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

# Build + upload release assets locally when hosted CI is unavailable
# (e.g. the Actions billing lock — ADR-0044). Builds every target this machine's
# toolchain supports, checksums each, and uploads them to the tag's *existing*
# GitHub release. Linux→Windows needs the x86_64-pc-windows-gnu target + mingw
# (`rustup target add x86_64-pc-windows-gnu` + `apt install gcc-mingw-w64-x86-64`).
# Usage: just release-local v0.18.1
release-local TAG:
    #!/usr/bin/env bash
    set -euo pipefail
    tag="{{TAG}}"
    [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: TAG must look like v0.18.1"; exit 1; }
    dist="$(mktemp -d)"; trap 'rm -rf "$dist"' EXIT
    assets=()
    build_one() {
      local target="$1" asset="$2" bin="$3"
      if ! rustup target list --installed | grep -qx "$target"; then
        echo "  skip $asset — rust target $target not installed"; return; fi
      echo "  building $asset ($target)…"
      if ! cargo build --release -p quiverdb-cli --target "$target" >/dev/null 2>&1; then
        echo "  skip $asset — build failed (missing linker/toolchain for $target)"; return; fi
      cp "target/$target/release/$bin" "$dist/$asset"
      ( cd "$dist" && sha256sum "$asset" > "$asset.sha256" )
      assets+=("$dist/$asset" "$dist/$asset.sha256")
      echo "  ✔ $asset"
    }
    build_one x86_64-unknown-linux-gnu  quiver-linux-x86_64       quiver
    build_one aarch64-unknown-linux-gnu quiver-linux-aarch64      quiver
    build_one x86_64-pc-windows-gnu     quiver-windows-x86_64.exe quiver.exe
    if [[ ${#assets[@]} -eq 0 ]]; then echo "error: no targets could be built on this host"; exit 1; fi
    cp docs/assets/icon/quiver-256.png "$dist/quiver-256.png"; assets+=("$dist/quiver-256.png")
    echo "uploading to release $tag:"; printf '  %s\n' "${assets[@]##*/}"
    gh release upload "$tag" "${assets[@]}" --clobber
    echo "done — verify at https://github.com/achref-soua/quiver/releases/tag/$tag"

# Build the container image.
docker:
    docker build -f infra/docker/Dockerfile -t quiver:dev .
