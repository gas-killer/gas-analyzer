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
[ -x "$INTERP" ] || { echo "missing $INTERP — build the portable tier first"; exit 1; }
# sp1-jit is x86_64-only: elsewhere (aarch64 Linux, Apple silicon) the default
# build IS the portable interpreter, so the matrix runs the one tier that
# exists and the cross-arch claim is carried by the cycle counts printed
# below matching the x86_64-recorded ones (tests/guest_e2e.rs pins them).
TIERS=("$INTERP")
if [ -x "$JIT" ] && [ "$("$JIT" --print-tier)" = "jit" ]; then
  TIERS=("$JIT" "$INTERP")
  echo "tiers: jit=$("$JIT" --print-tier) interp=$("$INTERP" --print-tier)"
else
  echo "tiers: interp only ($(uname -m): no jit tier on this host)"
fi
# guest|payload hex|extra args
CASES=(
  "hello-rs.elf|0x11223344|"
  "hello-c.elf|0x11223344|"
  "hello-c.elf|0x|"
  "hello-py.elf|0x11223344|"          # hello.py frozen into the MicroPython port
  "hello-py.elf|0x|"
  "bench-c.elf|0x0000000000989680|"   # N = 10,000,000
)

fail=0
for case in "${CASES[@]}"; do
  IFS='|' read -r guest payload extra <<<"$case"
  # Every (tier, run) must agree on "cycles output". (No associative arrays:
  # macOS ships bash 3.2.)
  reference=""
  results=""
  runs=0
  ok=1
  for tier_bin in "${TIERS[@]}"; do
    for i in $(seq 1 "$RUNS"); do
      out="$("$tier_bin" --program "$FIX/$guest" --input "$payload" $extra 2>/tmp/gk-run-stderr.$$)"
      cycles="$(grep -o '"cycles":[0-9]*' /tmp/gk-run-stderr.$$ | cut -d: -f2)"
      if [ -z "$reference" ]; then reference="$cycles $out"; fi
      if [ "$cycles $out" != "$reference" ]; then ok=0; fi
      results="$results  $("$tier_bin" --print-tier)/$i -> $cycles $out"$'\n'
      runs=$((runs + 1))
    done
  done
  cycles="${reference%% *}"
  if [ "$ok" = 1 ]; then
    echo "PASS $guest payload=$payload: $runs runs agree (cycles=$cycles)"
  else
    echo "FAIL $guest payload=$payload: divergent results:"
    printf '%s' "$results"
    fail=1
  fi
done
rm -f /tmp/gk-run-stderr.$$
exit "$fail"
