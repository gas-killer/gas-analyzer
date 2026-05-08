#!/usr/bin/env python3
"""Detect Criterion benchmark regressions by comparing two named baselines.

Walks target/criterion/ looking for <baseline>/estimates.json and
<candidate>/estimates.json files, compares mean wall-time point estimates,
writes a Markdown report, and exits non-zero if any metric regresses beyond
the given threshold.
"""

import argparse
import json
import sys
from pathlib import Path


def load_estimates(criterion_dir: Path, baseline: str) -> dict[str, float]:
    """Return bench_key → mean point estimate (nanoseconds) for a baseline."""
    results: dict[str, float] = {}
    for path in sorted(criterion_dir.rglob(f"{baseline}/estimates.json")):
        rel = path.relative_to(criterion_dir)
        # rel.parts = (<group>, [<subdir>...], <baseline>, "estimates.json")
        # Drop the last two parts (baseline name + filename) to get the bench key.
        bench_key = "/".join(rel.parts[:-2])
        try:
            data = json.loads(path.read_text())
            results[bench_key] = data["mean"]["point_estimate"]
        except (KeyError, json.JSONDecodeError, OSError):
            pass
    return results


def fmt_ns(ns: float) -> str:
    if ns >= 1e9:
        return f"{ns / 1e9:.3f} s"
    if ns >= 1e6:
        return f"{ns / 1e6:.3f} ms"
    if ns >= 1e3:
        return f"{ns / 1e3:.3f} µs"
    return f"{ns:.1f} ns"


def build_report(
    baseline: dict[str, float],
    candidate: dict[str, float],
    threshold: float,
    baseline_name: str,
) -> tuple[list[str], list[tuple[str, float, float, float]]]:
    """Return (report_lines, regressions)."""
    regressions: list[tuple[str, float, float, float]] = []
    table: list[str] = [
        f"| Benchmark | Baseline (`{baseline_name}`) | Candidate | Change |",
        "|-----------|:-----------------:|:---------:|:------:|",
    ]

    for key in sorted(set(baseline) | set(candidate)):
        if key not in baseline:
            table.append(f"| `{key}` | — | {fmt_ns(candidate[key])} | new |")
        elif key not in candidate:
            table.append(f"| `{key}` | {fmt_ns(baseline[key])} | — | skipped |")
        else:
            old, new = baseline[key], candidate[key]
            pct = (new - old) / old * 100
            if pct > threshold:
                status = f"⚠ **+{pct:.1f}%**"
                regressions.append((key, old, new, pct))
            elif pct < -2:
                status = f"✓ `{pct:.1f}%`"
            else:
                status = f"`{pct:+.1f}%`"
            table.append(
                f"| `{key}` | {fmt_ns(old)} | {fmt_ns(new)} | {status} |"
            )

    footer = (
        "<sub>Benchmarks run on shared GitHub-hosted runners. "
        f"Hardware variance may cause false positives — "
        f"only regressions >{threshold:.0f}% are flagged.</sub>"
    )

    if not baseline:
        summary = f"> No baseline found on `{baseline_name}` — results are for reference only."
    elif regressions:
        summary = (
            f"> **{len(regressions)} regression(s) detected** "
            f"(threshold: {threshold:.0f}%)"
        )
    else:
        summary = f"> No regressions detected (threshold: {threshold:.0f}%)."

    lines = ["## Benchmark Results", "", summary, ""] + table + ["", footer]
    return lines, regressions


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--threshold", type=float, default=15.0, metavar="PCT")
    ap.add_argument("--criterion-dir", default="target/criterion")
    ap.add_argument("--baseline", default="main")
    ap.add_argument("--candidate", default="new")
    ap.add_argument("--output-file", default="-", metavar="PATH")
    args = ap.parse_args()

    criterion_dir = Path(args.criterion_dir)
    baseline = load_estimates(criterion_dir, args.baseline)
    candidate = load_estimates(criterion_dir, args.candidate)

    if not baseline and not candidate:
        lines = ["## Benchmark Results", "", "> No benchmark data found."]
        _write("\n".join(lines) + "\n", args.output_file)
        return 0

    lines, regressions = build_report(baseline, candidate, args.threshold, args.baseline)
    _write("\n".join(lines) + "\n", args.output_file)

    if regressions:
        print("Regressions detected:", file=sys.stderr)
        for key, old, new, pct in regressions:
            print(
                f"  {key}: {fmt_ns(old)} → {fmt_ns(new)} (+{pct:.1f}%)",
                file=sys.stderr,
            )
        return 1

    return 0


def _write(text: str, output_file: str) -> None:
    if output_file == "-":
        print(text, end="")
    else:
        Path(output_file).write_text(text)


if __name__ == "__main__":
    sys.exit(main())
