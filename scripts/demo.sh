#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# One-command demo: build Quiver, start an encrypted-at-rest server on loopback,
# seed a small demo collection through the Python SDK, and print how to open the
# cockpit. Ctrl-C tears the server down. Dev-only — uses a throwaway data dir and
# generated keys.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REST_ADDR="${QUIVER_REST_ADDR:-127.0.0.1:6333}"
GRPC_ADDR="${QUIVER_GRPC_ADDR:-127.0.0.1:6334}"
API_KEY="${QUIVER_DEMO_API_KEY:-quiver-demo-key}"
ENC_KEY="${QUIVER_DEMO_ENCRYPTION_KEY:-$(openssl rand -hex 32)}"
DATA_DIR="$(mktemp -d)"

echo "==> building quiver"
cargo build -p quiverdb-cli

echo "==> starting encrypted server on ${REST_ADDR} (data: ${DATA_DIR})"
QUIVER_ENCRYPTION_KEY="$ENC_KEY" \
QUIVER_API_KEYS="$API_KEY" \
QUIVER_DATA_DIR="$DATA_DIR" \
QUIVER_REST_ADDR="$REST_ADDR" \
QUIVER_GRPC_ADDR="$GRPC_ADDR" \
  "$REPO/target/debug/quiver" serve &
SERVER_PID=$!
cleanup() { kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$DATA_DIR"; }
trap cleanup EXIT INT TERM

echo "==> waiting for readiness"
for _ in $(seq 1 100); do
  if curl -fsS "http://${REST_ADDR}/readyz" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

echo "==> seeding demo collection"
QUIVER_URL="http://${REST_ADDR}" QUIVER_API_KEY="$API_KEY" \
  uv run --quiet --project "$REPO/sdks/python" python "$REPO/scripts/seed_demo.py"

cat <<MSG

  ┌────────────────────────────────────────────────────────────┐
  │  Quiver demo is live — encryption-at-rest is ON.             │
  └────────────────────────────────────────────────────────────┘
    REST:     http://${REST_ADDR}
    API key:  ${API_KEY}

  Open the cockpit in another terminal:
    quiver tui --url http://${REST_ADDR} --api-key ${API_KEY}

  Press Ctrl-C to stop the server.

MSG

wait "$SERVER_PID"
