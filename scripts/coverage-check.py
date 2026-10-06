#!/usr/bin/env python3
"""Per-crate line coverage from an lcov file (spec 10 section 3: >= 80% on vk-proto, vk-hold,
vk-store, vk-agents, vk-compat).

  scripts/coverage-check.py lcov.info                  # table, exit 0
  scripts/coverage-check.py lcov.info --enforce        # exit 1 when a gated crate is below --min
  scripts/coverage-check.py lcov.info --crates vk-proto,vk-store --min 85

Writes a Markdown table to $GITHUB_STEP_SUMMARY when set. Exit codes: 0 ok (or report only),
1 below the threshold with --enforce, 2 input error.
"""
import argparse
import os
import re
import sys

GATED = ["vk-proto", "vk-hold", "vk-store", "vk-agents", "vk-compat"]


def parse(path):
    """crate -> (lines_found, lines_hit), from SF:/LF:/LH: records."""
    per = {}
    crate = None
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line.startswith("SF:"):
                m = re.search(r"(?:^|/)crates/([^/]+)/", line[3:])
                crate = m.group(1) if m else None
            elif line.startswith("LF:") and crate:
                per.setdefault(crate, [0, 0])[0] += int(line[3:])
            elif line.startswith("LH:") and crate:
                per.setdefault(crate, [0, 0])[1] += int(line[3:])
    return per


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("lcov")
    ap.add_argument("--min", type=float, default=80.0)
    ap.add_argument("--crates", default=",".join(GATED), help="comma-separated gated crates")
    ap.add_argument("--enforce", action="store_true")
    a = ap.parse_args(argv)
    try:
        per = parse(a.lcov)
    except OSError as e:
        print(f"coverage-check: {e}", file=sys.stderr)
        return 2
    if not per:
        print("coverage-check: no crate records in the lcov file", file=sys.stderr)
        return 2
    gated = [c for c in a.crates.split(",") if c]
    rows, below = [], []
    for crate in sorted(per):
        found, hit = per[crate]
        pct = 100.0 * hit / found if found else 100.0
        is_gated = crate in gated
        status = ""
        if is_gated:
            status = "ok" if pct >= a.min else "BELOW"
            if pct < a.min:
                below.append(crate)
        rows.append((crate, hit, found, pct, "gated " + status if is_gated else ""))
    for c in gated:
        if c not in per:
            rows.append((c, 0, 0, 0.0, "gated MISSING"))
            below.append(c)
    lines = [f"{'crate':14} {'lines':>14} {'cover':>7}  note"]
    for crate, hit, found, pct, note in rows:
        lines.append(f"{crate:14} {f'{hit}/{found}':>14} {pct:6.1f}%  {note}")
    print("\n".join(lines))
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(f"### Line coverage (gate: {a.min:.0f}% on {', '.join(gated)})\n\n")
            f.write("| crate | lines | coverage | note |\n|---|---|---|---|\n")
            for crate, hit, found, pct, note in rows:
                f.write(f"| {crate} | {hit}/{found} | {pct:.1f}% | {note} |\n")
    if below:
        msg = f"coverage-check: below {a.min:.0f}%: {', '.join(below)}"
        print(msg + ("" if a.enforce else "  (report only; pass --enforce to fail)"))
        return 1 if a.enforce else 0
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
