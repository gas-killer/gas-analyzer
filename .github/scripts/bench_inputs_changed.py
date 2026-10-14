#!/usr/bin/env python3
"""Report whether a PR changes anything the benchmarks measure.

The benchmarked package's workspace dependencies come from `cargo metadata`, so the set follows
Cargo.toml as crates are added. Files those crates compile in from outside their own directories
don't, and are listed in ALWAYS. A PR touching none of them, nor the build or bench setup, would
compare a build against itself, which only measures the runner.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

# Changes here can alter every crate's build, or how the benchmarks are run and judged.
ALWAYS = (
    "Cargo.toml",
    "Cargo.lock",
    "Makefile",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/",
    ".github/workflows/bench.yml",
    ".github/scripts/",
    # The estimator embeds its bytecode, which the benches run, from here.
    "abis/",
)


def benchmarked_dirs(package: str) -> list[str]:
    """The repo-relative directories of `package` and every workspace crate it depends on."""
    metadata = json.loads(
        subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"])
    )
    root = Path(metadata["workspace_root"])
    packages = {p["name"]: p for p in metadata["packages"]}
    seen: set[str] = set()
    todo = [package]
    while todo:
        name = todo.pop()
        if name in seen:
            continue
        seen.add(name)
        todo.extend(
            d["name"] for d in packages[name]["dependencies"] if d.get("path") and d["name"] in packages
        )
    return sorted(
        f"{Path(packages[n]['manifest_path']).parent.relative_to(root).as_posix()}/" for n in seen
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--base", required=True, help="commit the PR is compared against")
    ap.add_argument("--package", required=True, help="the package whose benchmarks run")
    args = ap.parse_args()

    changed = subprocess.check_output(
        ["git", "diff", "--name-only", f"{args.base}...HEAD"], text=True
    ).split()
    watched = (*ALWAYS, *benchmarked_dirs(args.package))
    hits = [f for f in changed if f.startswith(watched)]

    print(f"watched: {', '.join(watched)}", file=sys.stderr)
    print(f"changed and watched: {', '.join(hits) or 'none'}", file=sys.stderr)
    print(f"run={'true' if hits else 'false'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
