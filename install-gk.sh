#!/bin/sh
# gk installer — one line to a working gkvm toolchain:
#
#   curl -fsSL https://gaskiller.xyz/bash | sh
#
# Installs into $GK_HOME (default ~/.gk):
#   bin/gk-run   the guest executor sidecar, prebuilt for this OS/arch (GitHub release
#                assets of the `gk-run release` workflow, sha256-verified)
#   bin/gk-anvil anvil with the gkvm precompile (when the release carries it) — `gk anvil`
#   bin/gk       the `gk` command: runs solidity-sdk's tools/gk from the forge project you
#                are in (lib/solidity-sdk after `forge install gas-killer/solidity-sdk`, or
#                $GK_SDK), with GK_RUN pointed at the sidecar
# and pulls the prebuilt guest toolchain image (needs docker; skipped without it — `gk build`
# then uses ubuntu:24.04 + apt-get, slower but identical bytes).
#
# Env: GK_HOME, GK_RELEASE_TAG (pin a release; default: newest gk-run-* pre-release),
#      GK_TOOLCHAIN_IMAGE (default ghcr.io/gas-killer/gk-toolchain:v1), GK_NO_DOCKER=1,
#      GK_NO_MODIFY_PATH=1 (don't add $GK_HOME/bin to your shell profile; print the line instead).
set -eu

REPO="gas-killer/gas-analyzer"
GK_HOME="${GK_HOME:-$HOME/.gk}"
BIN="$GK_HOME/bin"
IMAGE="${GK_TOOLCHAIN_IMAGE:-ghcr.io/gas-killer/gk-toolchain:v1}"

say() { printf '%s\n' "$*" >&2; }
die() { say "gk install: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required"; }
need curl; need tar; need python3

os="$(uname -s)"; arch="$(uname -m)"
case "$os/$arch" in
  Linux/x86_64)            target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin/arm64)            target=aarch64-apple-darwin ;;
  *) die "no prebuilt gk-run for $os/$arch — build it: cargo install --locked --git https://github.com/$REPO gas-analyzer-gkvm --bin gk-run" ;;
esac

tag="${GK_RELEASE_TAG:-}"
if [ -z "$tag" ]; then
  tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=30" \
    | grep -o '"tag_name": *"gk-run-[^"]*"' | head -n1 | cut -d'"' -f4)"
  [ -n "$tag" ] || die "no gk-run release found on $REPO (set GK_RELEASE_TAG)"
fi
base="https://github.com/$REPO/releases/download/$tag"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
say "gk install: $tag → $BIN ($target)"
curl -fsSL -o "$tmp/gk-run.tar.gz" "$base/gk-run-$target.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
want="$(grep " gk-run-$target.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
if command -v sha256sum >/dev/null 2>&1; then have="$(sha256sum "$tmp/gk-run.tar.gz" | cut -d' ' -f1)"
else have="$(shasum -a 256 "$tmp/gk-run.tar.gz" | cut -d' ' -f1)"; fi
[ -n "$want" ] && [ "$want" = "$have" ] || die "sha256 mismatch for gk-run-$target.tar.gz"
mkdir -p "$BIN"
tar -xzf "$tmp/gk-run.tar.gz" -C "$BIN"
chmod +x "$BIN/gk-run"
say "  gk-run: $("$BIN/gk-run" --print-tier) tier"

# gk-anvil rides the same release when it was built (older pre-releases have none)
if curl -fsSL -o "$tmp/gk-anvil.tar.gz" "$base/gk-anvil-$target.tar.gz" 2>/dev/null; then
  want="$(grep " gk-anvil-$target.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
  if command -v sha256sum >/dev/null 2>&1; then have="$(sha256sum "$tmp/gk-anvil.tar.gz" | cut -d' ' -f1)"
  else have="$(shasum -a 256 "$tmp/gk-anvil.tar.gz" | cut -d' ' -f1)"; fi
  [ -n "$want" ] && [ "$want" = "$have" ] || die "sha256 mismatch for gk-anvil-$target.tar.gz"
  tar -xzf "$tmp/gk-anvil.tar.gz" -C "$BIN"
  chmod +x "$BIN/gk-anvil"
  say "  gk-anvil: installed"
else
  say "  gk-anvil: not in $tag (build it: cargo build --release --manifest-path crates/gk-anvil/Cargo.toml)"
fi

cat > "$BIN/gk" <<'SHIM'
#!/usr/bin/env bash
# `gk` — solidity-sdk's tools/gk, found from the forge project you are in.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export GK_RUN="${GK_RUN:-$here/gk-run}"
tools=""
if [ -n "${GK_SDK:-}" ] && [ -f "$GK_SDK/tools/gk/__main__.py" ]; then
  tools="$GK_SDK/tools/gk"
else
  dir="$PWD"
  while [ "$dir" != "/" ]; do
    if [ -f "$dir/lib/solidity-sdk/tools/gk/__main__.py" ]; then tools="$dir/lib/solidity-sdk/tools/gk"; break; fi
    if [ -f "$dir/tools/gk/__main__.py" ] && [ -f "$dir/src/gkvm/GkVm.sol" ]; then tools="$dir/tools/gk"; break; fi
    dir="$(dirname "$dir")"
  done
fi
if [ -z "$tools" ]; then
  echo "gk: no solidity-sdk found. In a forge project run: forge install gas-killer/solidity-sdk@gkvm-preview   (or set GK_SDK=/path/to/solidity-sdk)" >&2
  exit 2
fi
exec python3 -B "$tools" "$@"
SHIM
chmod +x "$BIN/gk"

if [ "${GK_NO_DOCKER:-0}" != "1" ] && command -v docker >/dev/null 2>&1; then
  if docker pull -q "$IMAGE" >/dev/null 2>&1; then say "  toolchain image: $IMAGE"
  else say "  toolchain image: could not pull $IMAGE (gk build will use ubuntu:24.04 + apt-get)"; fi
else
  say "  docker not found: gk build needs it (or a host riscv64-unknown-elf-gcc)"
fi

# Put $BIN on PATH in the profile of the user's login shell, as rustup and foundryup do (#200):
# piped into sh, this script can't prompt, and it can't change the calling shell's PATH, so a
# fresh `gk` is otherwise "command not found". Idempotent; GK_NO_MODIFY_PATH=1 opts out.
add_to_path() {
  case ":$PATH:" in *":$BIN:"*) return 0 ;; esac
  # The default location goes in as $HOME/..., so the line survives a moved home directory.
  case "$BIN" in
    "$HOME/.gk/bin") shown='$HOME/.gk/bin' ;;
    *) shown="$BIN" ;;
  esac
  now="export PATH=\"$BIN:\$PATH\""
  case "$(basename "${SHELL:-sh}")" in
    zsh)  profile="${ZDOTDIR:-$HOME}/.zshenv"; line="export PATH=\"$shown:\$PATH\"" ;;
    # macOS terminals start bash as a login shell, which reads .bash_profile, not .bashrc.
    bash) if [ "$os" = Darwin ]; then profile="$HOME/.bash_profile"; else profile="$HOME/.bashrc"; fi
          line="export PATH=\"$shown:\$PATH\"" ;;
    fish) profile="$HOME/.config/fish/config.fish"; line="fish_add_path -a \"$shown\""
          now="set -gx PATH \"$BIN\" \$PATH" ;;
    *)    profile="$HOME/.profile"; line="export PATH=\"$shown:\$PATH\"" ;;
  esac
  say ""
  if [ "${GK_NO_MODIFY_PATH:-0}" = "1" ]; then
    say "Add $BIN to PATH, e.g. in $profile:  $line"
    return 0
  fi
  if [ -f "$profile" ] && grep -qxF "$line" "$profile"; then
    say "PATH: $profile already adds $BIN"
  elif (mkdir -p "$(dirname "$profile")" &&
         printf '\n# gk (gaskiller.xyz/bash)\n%s\n' "$line" >>"$profile") 2>/dev/null; then
    say "PATH: added $BIN in $profile"
  else
    say "PATH: could not write $profile; add this line yourself:  $line"
    return 0
  fi
  say "  New shells pick it up. For this one:  $now"
}
add_to_path
say ""
say "Done. Next, in a forge project:"
say "  forge install gas-killer/solidity-sdk@gkvm-preview   # the tag that carries tools/gk (plain install once it is on main)"
say "  gk init --python      # a Python guest, its Solidity binding, a consumer and a test"
say "  gk test               # forge test with the guest really executing"
say "  gk anvil              # a local node where GkVm.exec works: cast call your consumer"
