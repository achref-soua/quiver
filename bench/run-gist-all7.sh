#!/usr/bin/env bash
# Benchmark ALL competitors on GIST1M (1M x 960, L2) sequentially.
#
# GIST1M at 960-d needs ~12 GB for the in-process competitors' (lancedb, chroma)
# index build, which OOM-kills on a 15.5 GB box. The fix is to AUGMENT MEMORY with
# a swapfile (one-time, needs sudo) so the build has virtual-memory headroom:
#
#   sudo fallocate -l 24G /swapfile && sudo chmod 600 /swapfile \
#     && sudo mkswap /swapfile && sudo swapon /swapfile
#   # (later, to remove: sudo swapoff /swapfile && sudo rm /swapfile)
#
# Then run this script. Competitors run one at a time (sequentially); the swap
# only provides headroom for the transient build peak. concurrency=1 avoids the
# qdrant-client cross-thread fd issue seen at high concurrency.
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."
unset VIRTUAL_ENV

if [ "$(awk '/SwapTotal/{print $2}' /proc/meminfo)" -lt 8000000 ]; then
  echo "WARNING: less than ~8 GiB swap detected; GIST1M in-process competitors may OOM." >&2
  echo "Add swap first (see the header of this script)." >&2
fi

DATA_DIR="$(mktemp -d /tmp/quiver-bench-gist-all7.XXXXXX)"
LOG=/tmp/gist1m_all7.log
: > "$LOG"
cargo build --release -p quiver-cli >>"$LOG" 2>&1 || { echo "BUILD FAILED" | tee -a "$LOG"; exit 1; }
QUIVER_REST_ADDR=127.0.0.1:7333 QUIVER_GRPC_ADDR=127.0.0.1:7334 \
QUIVER_INSECURE=true QUIVER_API_KEYS=bench-key QUIVER_DATA_DIR="$DATA_DIR" \
  target/release/quiver serve >>"$LOG" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null; rm -rf "$DATA_DIR"' EXIT
for i in $(seq 1 60); do
  curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:7333/healthz 2>/dev/null | grep -q 200 && break
  sleep 1
done
echo "[$(date +%T)] server ready; GIST1M all competitors (sequential) ..." | tee -a "$LOG"
bench/.venv/bin/python -m quiver_bench.comparison \
  --dataset gist1m \
  --competitors faiss,lancedb,chroma,milvus_server,qdrant,weaviate,quiver \
  --quiver-url http://127.0.0.1:7333 --quiver-key bench-key \
  --out docs/benchmarks/results/comparison-v0.18.0 \
  --ef 16,32,64,128,256 >>"$LOG" 2>&1
RC=$?
echo "[$(date +%T)] GIST1M all-7 exited rc=$RC" | tee -a "$LOG"
exit $RC
