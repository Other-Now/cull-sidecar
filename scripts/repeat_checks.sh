#!/usr/bin/env bash
# Repeats for the noisy measurements (shared laptop, WSL2): alternating order,
# medians taken in the README.   scripts/repeat_checks.sh <bin> <work dir>
set -euo pipefail
BIN=$(realpath "$1"); W=$(realpath -m "$2"); REPO=$(cd "$(dirname "$0")/.." && pwd); R="$REPO/results"
cd "$W"
echo "== scaling 1 vs 2 threads, alternating x3"
: > "$R/scaling_repeat.jsonl"
for rep in 1 2 3; do for t in 1 2; do
  rm -rf rep-scal; "$BIN" import --data rep-scal --dir bursts --threads "$t" | sed "s/^{/{\"rep\":$rep,/" | tee -a "$R/scaling_repeat.jsonl"
done; done; rm -rf rep-scal
echo "== ui latency 3 vs 4 threads, alternating x3"
: > "$R/ui_latency_repeat.jsonl"
for rep in 1 2 3; do for t in 3 4; do
  node "$REPO/bench/ui_latency.js" "$BIN" "$W" "$W/imagenette2-320/val" "$t" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.stringify(JSON.parse(s))))' | tee -a "$R/ui_latency_repeat.jsonl"
done; done
echo "== memory: re-import of the unchanged 20k library (no decoding: file list + catalog map only)"
"$BIN" import --data mem-20000 --dir lib-20000 --threads 3 | tee "$R/memory_reimport_20k.json"
echo done
