#!/usr/bin/env bash
# Reproduces every number in README.md. Linux (developed under WSL2).
#   scripts/run_benchmarks.sh <cull-sidecar binary> <work dir>
#   STAGES="memory sqlite" scripts/run_benchmarks.sh ...   # run a subset
# Needs node >= 22 on PATH. Downloads Imagenette (~340 MB) into the work dir.
# Note: no `| head` anywhere - with pipefail, head closing early SIGPIPEs the
# producer and kills the script (exit 141). awk 'NR<=n' reads to the end.
set -euo pipefail
BIN=$(realpath "$1"); W=$(realpath -m "$2"); REPO=$(cd "$(dirname "$0")/.." && pwd)
R="$REPO/results"; mkdir -p "$W" "$R"
STAGES=${STAGES:-"quality scaling memory sqlite ipc ui startup crash supervisor"}
want() { [[ " $STAGES " == *" $1 "* ]]; }
cd "$W"

if [ ! -d imagenette2-320 ]; then
  curl -sSfL -o imagenette2-320.tgz https://s3.amazonaws.com/fast-ai-imageclas/imagenette2-320.tgz
  tar xzf imagenette2-320.tgz
fi
[ -d bursts ]      || "$BIN" gen-bursts --src imagenette2-320/train --out bursts --bases 2000 --burst 4 --blur-p 0.3
[ -d bursts-mild ] || "$BIN" gen-bursts --src imagenette2-320/train --out bursts-mild --bases 2000 --burst 4 --blur-p 0.3 --sigma-lo 0.5 --sigma-hi 1.0 --seed 7

echo "{\"cpus\": $(nproc), \"model\": \"$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)\", \"kernel\": \"$(uname -r)\"}" | tee "$R/machine.json"

if want quality; then
  echo "== grouping + blur quality"
  for set in bursts bursts-mild; do
    rm -rf "cat-$set"; "$BIN" import --data "cat-$set" --dir "$set" >/dev/null
    "$BIN" eval --data "cat-$set" --labels "$set/labels.csv" > "$R/quality-$set.json"
  done
fi

if want scaling; then
  echo "== import scaling (8000 frames, warm page cache; order 1,2,3,4 then 4,3,2,1)"
  "$BIN" import --data scal-warm --dir bursts >/dev/null; rm -rf scal-warm
  : > "$R/import_scaling.jsonl"
  for t in 1 2 3 4 4 3 2 1; do
    rm -rf "scal-$t"; "$BIN" import --data "scal-$t" --dir bursts --threads "$t" | tee -a "$R/import_scaling.jsonl"
    rm -rf "scal-$t"
  done
fi

if want memory; then
  echo "== peak memory vs library size"
  rm -rf lib-all; mkdir lib-all
  find "$W/imagenette2-320" "$W/bursts" \( -name '*.JPEG' -o -name '*.jpg' \) | sort | awk 'NR<=20000' | \
    awk '{printf "ln -sf %s lib-all/%06d.jpg\n", $0, NR}' | sh
  : > "$R/memory.jsonl"
  for n in 1000 5000 20000; do
    rm -rf "lib-$n" "mem-$n"; mkdir "lib-$n"
    ls lib-all | awk -v n="$n" 'NR<=n' | while read -r f; do ln -s "$(readlink "lib-all/$f")" "lib-$n/$f"; done
    "$BIN" import --data "mem-$n" --dir "lib-$n" --threads 3 | sed "s/^{/{\"library\":$n,/" | tee -a "$R/memory.jsonl"
  done
fi

if want sqlite; then
  echo "== sqlite batching"
  : > "$R/sqlite_batching.jsonl"
  for s in full normal; do for b in 1 16 256; do
    "$BIN" bench-sqlite --data sqlite-bench --rows 5000 --batch "$b" --sync "$s" | tee -a "$R/sqlite_batching.jsonl"
  done; done
fi

if want ipc; then
  echo "== ipc (3 run orders)"
  : > "$R/ipc_transports.jsonl"
  for o in stdio,uds,tcp tcp,uds,stdio uds,stdio,tcp; do
    NO_BLOBS=1 node "$REPO/bench/ipc.js" "$BIN" "$W" "$o" 2>/dev/null | \
      node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.stringify({order:process.argv[1],...JSON.parse(s).transports})))' "$o" | \
      tee -a "$R/ipc_transports.jsonl"
  done
  node "$REPO/bench/ipc.js" "$BIN" "$W" stdio 2>/dev/null > "$R/ipc_blobs.json"
fi

if want ui; then
  echo "== ui latency during import (val set)"
  for t in 3 4; do node "$REPO/bench/ui_latency.js" "$BIN" "$W" "$W/imagenette2-320/val" "$t" > "$R/ui_latency-t$t.json"; cat "$R/ui_latency-t$t.json"; done
fi

if want startup; then
  echo "== startup"
  node "$REPO/bench/startup.js" "$BIN" "$W" 2>/dev/null > "$R/startup.json"
fi

if want crash; then
  echo "== durability: 10 x kill -9 mid-edit"
  node "$REPO/tests/crash.js" "$BIN" "$W/crash-safe" --kills 10 2>/dev/null > "$R/crash-safe.json"
  node "$REPO/tests/crash.js" "$BIN" "$W/crash-unsafe" --kills 10 --unsafe 2>/dev/null > "$R/crash-unsafe.json"
fi

if want supervisor; then
  echo "== supervisor"
  RESULTS="$R" CULL_BIN="$BIN" node --test "$REPO/tests/supervisor.test.js"
fi
echo done
