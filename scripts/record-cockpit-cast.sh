#!/usr/bin/env bash
# Record the retro cockpit — including the constellation view — to an asciinema
# cast at docs/assets/cockpit.cast.
#
# This captures an INTERACTIVE terminal session, so it must run on a real TTY; it
# cannot run in CI or a non-interactive environment (it exits early there rather
# than produce an empty cast).
#
# Prerequisites:
#   - asciinema (https://asciinema.org)
#   - a `quiver` binary on PATH: `cargo install --path crates/quiver-cli`
#     (or run `just build` and use ./target/debug/quiver)
#   - a running server with a demo collection — `just demo` starts one with
#     encryption-at-rest on and prints the API key (default below).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CAST="$REPO_ROOT/docs/assets/cockpit.cast"
URL="${QUIVER_URL:-http://127.0.0.1:6333}"
API_KEY="${QUIVER_API_KEY:-quiver-demo-key}"

command -v asciinema >/dev/null || {
  echo "asciinema not found — install it: https://asciinema.org" >&2
  exit 1
}
command -v quiver >/dev/null || {
  echo "quiver not on PATH — 'cargo install --path crates/quiver-cli', or use ./target/debug/quiver" >&2
  exit 1
}
if [ ! -t 0 ]; then
  echo "stdin is not a TTY — record the cast from an interactive terminal" >&2
  exit 1
fi

mkdir -p "$REPO_ROOT/docs/assets"
cat <<'TIPS'
Recording the Quiver cockpit. Once it opens, a good tour is:
  • ↑/↓ to select a collection, then 'v' (or enter) to open the constellation
  • ↑/↓ to move the cursor between points; the nearest neighbour is the bright star
  • 'enter' to re-query around the selected point; 'esc' to go back; 'q' to quit
TIPS

exec asciinema rec "$CAST" --overwrite --title "Quiver cockpit" \
  --command "quiver tui --url '$URL' --api-key '$API_KEY'"
