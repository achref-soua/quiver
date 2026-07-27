#!/usr/bin/env bash
# Record the retro cockpit — including the constellation view — to an asciinema
# cast at docs/assets/cockpit.cast.
#
# Two modes:
#   - on a real TTY, this records an INTERACTIVE session so you can give the tour
#     yourself (the default; pass --auto to override);
#   - anywhere else — CI, a headless shell, an agent — it delegates to
#     scripts/record_cockpit_cast.py, which allocates a pty and drives a scripted
#     tour. A pty is a terminal, which is all asciinema and crossterm need, so the
#     cast is a regenerable artifact rather than a hand-made one that goes stale.
#
# Prerequisites:
#   - asciinema (https://asciinema.org)
#   - a `quiver` binary on PATH: `cargo install --path crates/quiver-cli`
#     (or run `just build` and use ./target/debug/quiver)
#   - a running server with a demo collection — `just demo` starts one with
#     encryption-at-rest on and prints the API key (default below).
#   - for the scripted mode: uv (the driver seeds through the Python SDK).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CAST="$REPO_ROOT/docs/assets/cockpit.cast"
URL="${QUIVER_URL:-http://127.0.0.1:6333}"
API_KEY="${QUIVER_API_KEY:-quiver-demo-key}"

command -v asciinema >/dev/null || {
  echo "asciinema not found — install it: https://asciinema.org" >&2
  exit 1
}
# Prefer this checkout's own build: the cast is a repo artifact, so it must depict
# the source it is committed beside — not whatever older `quiver` happens to be
# installed on PATH. Override with QUIVER_BIN.
if [ -z "${QUIVER_BIN:-}" ]; then
  for candidate in "$REPO_ROOT/target/release/quiver" "$REPO_ROOT/target/debug/quiver"; do
    [ -x "$candidate" ] && QUIVER_BIN="$candidate" && break
  done
fi
[ -n "${QUIVER_BIN:-}" ] || QUIVER_BIN="$(command -v quiver || true)"
[ -x "${QUIVER_BIN:-}" ] || {
  echo "quiver not found — run 'just build', or set QUIVER_BIN" >&2
  exit 1
}
echo "==> recording with $QUIVER_BIN ($("$QUIVER_BIN" --version))"

mkdir -p "$REPO_ROOT/docs/assets"

# Scripted (headless) mode: not a TTY, or explicitly asked for.
if [ ! -t 0 ] || [ "${1:-}" = "--auto" ]; then
  command -v uv >/dev/null || {
    echo "uv not found — needed to seed the cast's collections; see https://docs.astral.sh/uv/" >&2
    exit 1
  }
  exec uv run --quiet --project "$REPO_ROOT/sdks/python" \
    python "$REPO_ROOT/scripts/record_cockpit_cast.py" \
    --cast "$CAST" --url "$URL" --api-key "$API_KEY" --quiver "$QUIVER_BIN"
fi

cat <<'TIPS'
Recording the Quiver cockpit. Once it opens, a good tour is:
  • ↑/↓ to select a collection, then 'v' (or enter) to open the constellation
  • ↑/↓ to move the cursor between points; the nearest neighbour is the bright star
  • 'enter' to re-query around the selected point; 'esc' to go back; 'q' to quit
TIPS

exec asciinema rec "$CAST" --overwrite --title "Quiver cockpit" \
  --command "$QUIVER_BIN tui --url '$URL' --api-key '$API_KEY'"
