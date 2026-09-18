#!/usr/bin/env bash
# Runs INSIDE the setcode-bench container (see Makefile). Per sample: a fresh
# vanilla anvil with the e2e's flags (script/e2e_operator_replay.sh), then the
# unmodified deploy_anvil.py --overlay over /artifacts, timed as one process —
# file reads, keccak, hex/JSON encoding and the batched anvil_setCode calls are
# all part of the e2e's mount step.
set -euo pipefail

SAMPLES="${SAMPLES:-3}"
PORT=8560
RPC="http://127.0.0.1:$PORT"

anvil --version | head -1

for sample in $(seq 0 $((SAMPLES - 1))); do
  anvil --gas-limit 1099511627776 --port "$PORT" --silent &
  ANVIL_PID=$!
  until python3 -c "import urllib.request,json; urllib.request.urlopen(urllib.request.Request('$RPC', json.dumps({'jsonrpc':'2.0','id':1,'method':'eth_chainId','params':[]}).encode(), {'Content-Type':'application/json'}))" 2>/dev/null; do
    sleep 0.1
  done

  t0=$(date +%s%N)
  python3 /bench/deploy_anvil.py --artifacts /artifacts --rpc "$RPC" --overlay > /tmp/deploy.log
  t1=$(date +%s%N)

  chunks=$(head -1 /tmp/deploy.log | cut -d' ' -f1)
  manifest=$(grep 'overlay manifest:' /tmp/deploy.log | cut -d' ' -f3)
  rss_kb=$(grep VmHWM "/proc/$ANVIL_PID/status" | tr -s ' \t' ' ' | cut -d' ' -f2)
  echo "{\"leg\":\"v2-setcode\",\"sample\":$sample,\"wall_nanos\":$((t1 - t0)),\"set_code_calls\":$chunks,\"overlay_manifest\":\"$manifest\",\"anvil_peak_rss_kb\":$rss_kb}"

  kill "$ANVIL_PID"
  wait "$ANVIL_PID" 2>/dev/null || true
done
