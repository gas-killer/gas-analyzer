#!/usr/bin/env bash
# UNBOUNDED_V3 M4 — the flagship measurement: the REAL Qwen3-0.6B answer through the
# committed qwen-c.elf guest, against the release bytes the V2 consumer settles on-chain
# (gas-killer/solidity-sdk release qwen3-0.6b-onchain-v1).
#
#   scripts/flagship/run.sh [maxNewTokens ...]      (default: 1 8)
#
# Env: GK_RUN (default target/release/gk-run), GK_QWEN_DIR (default target/qwen-real;
# weights.bin + tokenizer.bin are downloaded there when missing).
# Prints one JSON line per case on stdout; exits nonzero on any mismatch or failed run.
# Baseline for comparison (V2, pure Solidity under revm): 8-token answer = 545.1B gas, ~9 min.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
GK_RUN="${GK_RUN:-$ROOT/target/release/gk-run}"
DIR="${GK_QWEN_DIR:-$ROOT/target/qwen-real}"
ELF="$ROOT/crates/gkvm/tests/fixtures/qwen-c.elf"
RELEASE="https://github.com/gas-killer/solidity-sdk/releases/download/qwen3-0.6b-onchain-v1"
# On-chain identity of these bytes under V2: overlay manifest
# 0x23216cb9ed9ef2b4bc20c84d27b68fa62ab194fc0845dfa707836f48ec4a7ae9
# = keccak256(keccak256(weights.bin) || keccak256(tokenizer.bin)).
# Qwen3Engine's packedConfig for this model, and the chat-templated prompt
# "<|im_start|>user\nWhat is Ethereum?<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
# (both as in crates/gk-fast-view/examples/real_repro.rs).
CONFIG=(
  04000c001c100800800002518004000101000000000000000000000000000000
  0000000010c6f7a10000000016a09e6600000000239791f10000000000000000
  00182bc20002505d0002505b0000000000000000000000000000000000000000
)
PROMPT=(151644 872 198 3838 374 33946 30 151645 198 151644 77091 198 151667 271 151668 271)

mkdir -p "$DIR"
for f in weights.bin tokenizer.bin; do
  [ -s "$DIR/$f" ] || curl -fsSL -o "$DIR/$f" "$RELEASE/$f"
done
# The release bytes, pinned (sha256 — available everywhere), and their manifest-v3 root (same bytes, new root).
check_sha() { [ "$(shasum -a 256 "$1" | cut -d' ' -f1)" = "$2" ] || { echo "sha256 mismatch: $1" >&2; exit 1; }; }
check_sha "$DIR/weights.bin" 7135c9509db58a12f80671d409e528b4f7cc45bbdf9c5c8ee737fc954297db8a
check_sha "$DIR/tokenizer.bin" ec813734e9e01a2784e7a2c9ee68b39c0a42a57e6b2bcc0e9a7a6f00d1041dc0
ARTIFACT_ROOT=0xad3abf5617f9c7e1862d7a3e0a2cf368939e09ae69f094bb2e3bf42279e99115
BUNDLE="$DIR/weights.bin,$DIR/tokenizer.bin"
root="$("$GK_RUN" --print-artifact-root --artifact "$BUNDLE")"
[ "$root" = "$ARTIFACT_ROOT" ] || { echo "artifact root $root != pinned $ARTIFACT_ROOT" >&2; exit 1; }

word() { printf '%064x' "$1"; }
payload() { # abi.encode(bytes32[3] packedConfig, uint32[] promptIds, uint256 maxNewTokens)
  local out
  out="0x${CONFIG[0]}${CONFIG[1]}${CONFIG[2]}$(word 160)$(word "$1")$(word "${#PROMPT[@]}")"
  for id in "${PROMPT[@]}"; do out="$out$(word "$id")"; done
  printf '%s' "$out"
}

tier="$("$GK_RUN" --print-tier)"
[ "$#" -gt 0 ] || set -- 1 8
for max_new in "$@"; do
  report="$(mktemp)"
  out="$("$GK_RUN" --program "$ELF" --artifact "$BUNDLE" --artifact-root "$ARTIFACT_ROOT" --schedule sequential --deadline-secs 0 \
    --input "$(payload "$max_new")" 2>"$report")" || { cat "$report" >&2; echo "gk-run failed (maxNew=$max_new)" >&2; exit 1; }
  # abi.encode(string answer, uint32[] answerIds): decode the string for the log.
  answer="$(cast abi-decode --input 'f(string,uint32[])' "$out" 2>/dev/null | head -n1 || true)"
  cycles="$(grep -o '"cycles":[0-9]*' "$report" | cut -d: -f2)"
  gas="$(grep -o '"gas_used":[0-9]*' "$report" | cut -d: -f2)"
  wall="$(grep -o '"wall_nanos":[0-9]*' "$report" | cut -d: -f2)"
  printf '{"case":"real-qwen3-0.6b","max_new":%s,"tier":"%s","cycles":%s,"gas":%s,"wall_nanos":%s,"output_keccak":"%s","answer":%s}\n' \
    "$max_new" "$tier" "$cycles" "$gas" "$wall" "$(cast keccak "$out")" "${answer:-null}"
  rm -f "$report"
done
