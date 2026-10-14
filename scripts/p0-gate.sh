#!/usr/bin/env bash
# P0 feasibility gate: load real llama-server instances while hearth-probe
# records host and per-process memory, so `hearth-probe report` can check
# whether measured signals are good enough to admit work on.
#
# usage: scripts/p0-gate.sh OUT_DIR MODEL_A.gguf MODEL_B.gguf [-- extra llama-server args]
#
# Environment (all optional; each value is an experiment parameter, recorded in
# the marks so the report states what was run):
#   LLAMA_SERVER   llama-server binary            (default: from PATH)
#   CTX            context per server             (default: 8192)
#   BASELINE_S     idle sampling before loads     (default: 60)
#   SETTLE_S       wait after ready before next   (default: 20)
#   RECLAIM_S      wait after unload              (default: 30)
#   FILL_TOKENS    approximate prompt tokens sent to A to exercise the KV cache
#                  (default: CTX / 2)
#
# Only processes this script starts are ever signalled.

set -euo pipefail

if [[ $# -lt 3 ]]; then
  sed -n '2,20p' "$0"
  exit 2
fi
OUT=$1 MODEL_A=$2 MODEL_B=$3
shift 3
[[ ${1:-} == "--" ]] && shift
EXTRA=("$@")

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PROBE=${PROBE:-$ROOT/target/release/hearth-probe}
LLAMA_SERVER=${LLAMA_SERVER:-$(command -v llama-server)}
CTX=${CTX:-8192}
BASELINE_S=${BASELINE_S:-60}
SETTLE_S=${SETTLE_S:-20}
RECLAIM_S=${RECLAIM_S:-30}
FILL_TOKENS=${FILL_TOKENS:-$((CTX / 2))}

mkdir -p "$OUT"
SAMPLES=$OUT/samples.jsonl
PIDS=$OUT/pids
: >"$PIDS"
STARTED=()

mark() { "$PROBE" mark --out "$SAMPLES" "$@"; }

cleanup() {
  for pid in "${STARTED[@]}"; do kill -TERM "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'
}

file_bytes() { stat -f %z "$1" 2>/dev/null || stat -c %s "$1"; }

wait_ready() { # port pid
  until curl -fsS "http://127.0.0.1:$1/health" >/dev/null 2>&1; do
    kill -0 "$2" 2>/dev/null || { echo "llama-server pid $2 exited before ready" >&2; return 1; }
    sleep 0.2
  done
}

start_server() { # label model
  local label=$1 model=$2 port pid log
  port=$(free_port)
  log=$OUT/$label.log
  mark "${label}_start" "file_bytes=$(file_bytes "$model")" "model=$model" "ctx=$CTX" "port=$port" "log=$log"
  "$LLAMA_SERVER" -m "$model" --host 127.0.0.1 --port "$port" -c "$CTX" -np 1 "${EXTRA[@]}" >"$log" 2>&1 &
  pid=$!
  STARTED+=("$pid")
  echo "$pid" >>"$PIDS"
  wait_ready "$port" "$pid"
  mark "${label}_ready" "pid=$pid" "port=$port"
  sleep "$SETTLE_S"
  mark "${label}_settled" "pid=$pid"
  eval "${label}_PID=$pid ${label}_PORT=$port"
}

"$PROBE" sample --interval-ms 100 --pids-from "$PIDS" --out "$SAMPLES" &
PROBE_PID=$!
STARTED+=("$PROBE_PID")

mark run_start "ctx=$CTX" "baseline_s=$BASELINE_S" "settle_s=$SETTLE_S" "reclaim_s=$RECLAIM_S" "extra=${EXTRA[*]:-}"
mark baseline_start
sleep "$BASELINE_S"
mark baseline_end

start_server a "$MODEL_A"
start_server b "$MODEL_B"

# Exercise the KV cache on A: llama.cpp reserves it at load, so footprint should
# not grow much here. If it does, admission must reserve per request instead.
mark fill_start "pid=$a_PID" "tokens=$FILL_TOKENS"
python3 - "$a_PORT" "$FILL_TOKENS" <<'PY'
import json, sys, urllib.request
port, tokens = sys.argv[1], int(sys.argv[2])
prompt = " ".join(["hearth"] * tokens)
req = urllib.request.Request(
    f"http://127.0.0.1:{port}/completion",
    data=json.dumps({"prompt": prompt, "n_predict": 16, "cache_prompt": False}).encode(),
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(req, timeout=600) as r:
    body = json.load(r)
print(json.dumps({k: body.get(k) for k in ("tokens_evaluated", "truncated")}))
PY
mark fill_end "pid=$a_PID"
sleep "$SETTLE_S"
mark fill_settled "pid=$a_PID"

mark a_unload "pid=$a_PID"
kill -TERM "$a_PID"
wait "$a_PID" 2>/dev/null || true
sleep "$RECLAIM_S"
mark a_reclaimed

mark b_unload "pid=$b_PID"
kill -TERM "$b_PID"
wait "$b_PID" 2>/dev/null || true
sleep "$RECLAIM_S"
mark b_reclaimed
mark run_end

kill -TERM "$PROBE_PID" 2>/dev/null || true
wait "$PROBE_PID" 2>/dev/null || true
echo "recorded $SAMPLES"
