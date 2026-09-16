#!/usr/bin/env bash
# M1 determinism matrix: {interp, jit} × N runs × every fixture guest —
# byte-identical stdout AND identical instruction counts, or nonzero exit.
#
#   scripts/m1-matrix.sh [runs]           (default 10)
#
# Expects both tier binaries built in release:
#   cargo build --release -p gas-analyzer-gkvm --bin gk-run
#   cargo build --release -p gas-analyzer-gkvm --bin gk-run \
#     --features gas-analyzer-gkvm/portable-exec --target-dir target/portable
set -euo pipefail
RUNS="${1:-10}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
FIX="$ROOT/crates/gkvm/tests/fixtures"
JIT="$ROOT/target/release/gk-run"
INTERP="$ROOT/target/portable/release/gk-run"

for bin in "$JIT" "$INTERP"; do
  [ -x "$bin" ] || { echo "missing $bin — build both tiers first"; exit 1; }
done
echo "tiers: jit=$("$JIT" --print-tier) interp=$("$INTERP" --print-tier)"

# guest|payload hex|extra args
CASES=(
  "hello-rs.elf|0x11223344|"
  "hello-c.elf|0x11223344|"
  "hello-c.elf|0x|"
  "bench-c.elf|0x0000000000989680|"   # N = 10,000,000
)

fail=0
for case in "${CASES[@]}"; do
  IFS='|' read -r guest payload extra <<<"$case"
  declare -A seen=()
  for tier_bin in "$JIT" "$INTERP"; do
    tier="$("$tier_bin" --print-tier)"
    for i in $(seq 1 "$RUNS"); do
      out="$("$tier_bin" --program "$FIX/$guest" --input "$payload" $extra 2>/tmp/gk-run-stderr.$$)"
      cycles="$(grep -o '"cycles":[0-9]*' /tmp/gk-run-stderr.$$ | cut -d: -f2)"
      seen["$tier/$i"]="$cycles $out"
    done
  done
  # Every (tier, run) must agree on "cycles output".
  reference=""
  ok=1
  for key in "${!seen[@]}"; do
    if [ -z "$reference" ]; then reference="${seen[$key]}"; fi
    if [ "${seen[$key]}" != "$reference" ]; then ok=0; fi
  done
  cycles="${reference%% *}"
  if [ "$ok" = 1 ]; then
    echo "PASS $guest payload=$payload: ${#seen[@]} runs agree (cycles=$cycles)"
  else
    echo "FAIL $guest payload=$payload: divergent results:"
    for key in "${!seen[@]}"; do echo "  $key -> ${seen[$key]}"; done | sort
    fail=1
  fi
  unset seen
done
rm -f /tmp/gk-run-stderr.$$
exit "$fail"
