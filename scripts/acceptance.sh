#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Real-user acceptance run (Phase A of the v0.17.0 hardening pass). Boots a real
# Quiver server with encryption-at-rest ON, then drives every external surface as
# an operator would and asserts correctness:
#
#   * REST     — curl: health, CRUD, hybrid search, and an RBAC denial.
#   * Python SDK — scripts/acceptance_sdk.py: every index kind, PQ, both
#                  client-side encryption modes, and multi-vector/ColBERT.
#   * CLI      — `quiver admin import`: offline import into an encrypted db, and
#                the cleartext-credential warning.
#   * MCP      — newline-delimited JSON-RPC over stdio: initialize, tools/list,
#                create_collection, upsert, search.
#
# The gRPC surface and the TUI cockpit are covered by the Rust integration tests
# (`crates/quiver-server/tests/*.rs`, `crates/quiver-tui/tests/live.rs`); see
# docs/testing/manual-acceptance.md for the full surface map.
#
# Dev-only: throwaway data dirs, generated keys, loopback alt ports. Requires a
# Rust toolchain, `uv`, `curl`, and `openssl`. Run from the repo root:  ./scripts/acceptance.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REST_ADDR="${QUIVER_ACC_REST_ADDR:-127.0.0.1:7333}"
GRPC_ADDR="${QUIVER_ACC_GRPC_ADDR:-127.0.0.1:7334}"
API_KEY="${QUIVER_ACC_API_KEY:-acc-admin-key}"
ENC_KEY="$(openssl rand -hex 32)"
DATA_DIR="$(mktemp -d)"
IMPORT_DIR="$(mktemp -d)"
MCP_DIR="$(mktemp -d)"
SERVER_PID=""

red() { printf '\033[31m%s\033[0m\n' "$*"; }
grn() { printf '\033[32m%s\033[0m\n' "$*"; }
hdr() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$DATA_DIR" "$IMPORT_DIR" "$MCP_DIR"
}
trap cleanup EXIT INT TERM

fail() { red "FAIL: $*"; exit 1; }
pass() { grn "  ok: $*"; }

hdr "build"
cargo build -q -p quiver-cli

hdr "boot encrypted server on ${REST_ADDR}"
QUIVER_ENCRYPTION_KEY="$ENC_KEY" \
QUIVER_API_KEYS="$API_KEY" \
QUIVER_DATA_DIR="$DATA_DIR" \
QUIVER_REST_ADDR="$REST_ADDR" \
QUIVER_GRPC_ADDR="$GRPC_ADDR" \
  "$REPO/target/debug/quiver" serve &
SERVER_PID=$!

for _ in $(seq 1 150); do
  curl -fsS "http://${REST_ADDR}/readyz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "http://${REST_ADDR}/readyz" >/dev/null 2>&1 || fail "server did not become ready"
pass "server ready (encryption-at-rest ON)"

AUTH=(-H "Authorization: Bearer ${API_KEY}")
BASE="http://${REST_ADDR}"

hdr "REST surface (curl)"
# Create -> upsert -> hybrid search -> get -> delete -> drop.
curl -fsS "${AUTH[@]}" -X POST "${BASE}/v1/collections" \
  -H 'content-type: application/json' \
  -d '{"name":"rest_acc","dim":4,"metric":"cosine","filterable":[{"path":"topic","field_type":"keyword"}]}' >/dev/null \
  || fail "create collection"
pass "created collection"

curl -fsS "${AUTH[@]}" -X POST "${BASE}/v1/collections/rest_acc/points" \
  -H 'content-type: application/json' \
  -d '{"points":[
        {"id":"a","vector":[0.9,0.1,0.0,0.0],"payload":{"topic":"search"}},
        {"id":"b","vector":[0.0,0.9,0.1,0.0],"payload":{"topic":"storage"}},
        {"id":"c","vector":[0.0,0.0,0.9,0.1],"payload":{"topic":"ops"}}]}' >/dev/null \
  || fail "upsert points"
pass "upserted 3 points"

HITS="$(curl -fsS "${AUTH[@]}" -X POST "${BASE}/v1/collections/rest_acc/query" \
  -H 'content-type: application/json' \
  -d '{"vector":[0.9,0.1,0.0,0.0],"k":1,"filter":{"eq":{"field":"topic","value":"storage"}}}')"
echo "$HITS" | grep -q '"id":"b"' || fail "hybrid search expected id b, got: $HITS"
pass "hybrid (pre-filtered) search returned the storage point"

curl -fsS "${AUTH[@]}" "${BASE}/v1/collections/rest_acc/points/a" | grep -q '"id":"a"' \
  || fail "get point a"
pass "get point round-trips"

# RBAC: a wrong key must be denied (401).
CODE="$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong-key' \
  "${BASE}/v1/collections")"
[ "$CODE" = "401" ] || fail "expected 401 for a wrong key, got ${CODE}"
pass "wrong API key is rejected (401)"

# RBAC: no key at all must be denied too.
CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/v1/collections")"
[ "$CODE" = "401" ] || fail "expected 401 for a missing key, got ${CODE}"
pass "missing API key is rejected (401)"

curl -fsS "${AUTH[@]}" -X DELETE "${BASE}/v1/collections/rest_acc" >/dev/null || fail "drop collection"
pass "dropped collection"

hdr "Python SDK surface (every index kind, PQ, both encrypted modes, multi-vector)"
QUIVER_URL="$BASE" QUIVER_API_KEY="$API_KEY" \
  uv run --quiet --project "$REPO/sdks/python" python "$REPO/scripts/acceptance_sdk.py" \
  || fail "Python SDK acceptance"

hdr "CLI surface (quiver admin import, encrypted-at-rest)"
EXPORT="$IMPORT_DIR/qdrant.jsonl"
cat >"$EXPORT" <<'JSONL'
{"id": 1, "vector": [1.0, 0.0, 0.0], "payload": {"city": "paris"}}
{"id": 2, "vector": [0.0, 1.0, 0.0], "payload": {"city": "rome"}}
{"id": 3, "vector": [0.0, 0.0, 1.0], "payload": {"city": "oslo"}}
JSONL
OUT="$(QUIVER_ENCRYPTION_KEY="$ENC_KEY" "$REPO/target/debug/quiver" admin import \
  --source qdrant --input "$EXPORT" --collection places \
  --data-dir "$IMPORT_DIR/data" --metric l2 --filterable city:keyword)"
echo "$OUT" | grep -q "imported 3 points" || fail "import output: $OUT"
pass "imported 3 points into an encrypted db"

# The cleartext-credential warning must fire for an api-key over http:// (the
# fetch then fails fast against a dead port — we only assert the warning).
WARN="$(QUIVER_ENCRYPTION_KEY="$ENC_KEY" "$REPO/target/debug/quiver" admin import \
  --source qdrant --qdrant-url 'http://127.0.0.1:59999' --api-key 'secret-key' \
  --collection nope --data-dir "$IMPORT_DIR/data2" --metric l2 2>&1 || true)"
echo "$WARN" | grep -qi "cleartext" || fail "expected a cleartext-credential warning, got: $WARN"
pass "cleartext-credential warning fires for an api-key over http://"

hdr "MCP surface (JSON-RPC over stdio)"
# The `upsert` tool takes one point per call (id + vector), not a batch.
MCP_OUT="$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_collection","arguments":{"name":"mcp_acc","dim":3}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"upsert","arguments":{"collection":"mcp_acc","id":"x","vector":[1.0,0.0,0.0]}}}' \
  '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"upsert","arguments":{"collection":"mcp_acc","id":"y","vector":[0.0,1.0,0.0]}}}' \
  '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search","arguments":{"collection":"mcp_acc","vector":[0.9,0.1,0.0],"k":1}}}' \
  | "$REPO/target/debug/quiver" mcp --insecure --data-dir "$MCP_DIR/data")"
echo "$MCP_OUT" | grep -q '"create_collection"' || fail "tools/list missing create_collection"
pass "tools/list exposes the tool set"
# The search result is JSON embedded in a text field, so the id is escaped: \"id\":\"x\".
echo "$MCP_OUT" | grep -qF '\"id\":\"x\"' || fail "MCP search did not return point x: $MCP_OUT"
pass "create_collection + upsert + search round-trips over MCP"

hdr "ACCEPTANCE PASSED"
grn "All surfaces (REST, Python SDK, CLI, MCP) exercised against a live encrypted server."
