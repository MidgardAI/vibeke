#!/usr/bin/env python3
"""Relative performance gate for noisy shared runners (spec 10 section 2.2 PR gate).

Absolute milliseconds mean nothing on a shared CI runner, so the gate never compares them. Each
metric is divided by a *calibration* measured on the same machine in the same sample (a fixed
CPU workload and a fixed process-spawn workload), which cancels the runner's speed, and the
median over several samples is compared with the stored baseline ratio. A run only fails when
the median ratio is worse than the baseline by more than the tolerance (default 100 percent,
i.e. 2x), and never when the samples disagree wildly among themselves (noisy host: reported,
not failed) or when there is no baseline for the platform (reported, candidate written).

  perf_compare.py calibrate                      print {"cpu_ms":..,"spawn_ms":..} as JSON
  perf_compare.py compare SAMPLES_DIR [--baseline tests/perf/baseline.json] [--tolerance 1.0]
                                      [--update] [--candidate OUT.json]

SAMPLES_DIR holds one directory per sample: sN/timing.tsv ("name<TAB>ms" per line, as written
by `VIBEKE_TIMING_REPORT`), sN/vt.txt (optional, VT throughput in MB/s) and sN/cal.json (the
output of `calibrate`). Exit codes: 0 pass or report-only, 1 regression, 2 usage/input error.
"""
import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import sys
import time

# metric -> (calibrator, better). Timings are lower-better and scale with process-spawn and
# I/O cost; VT throughput is higher-better and scales with CPU speed.
METRICS = {
    "cold_start": ("spawn_ms", "lower"),
    "attach": ("spawn_ms", "lower"),
    "restart_30_panes": ("spawn_ms", "lower"),
    "reboot_restore_30_panes": ("spawn_ms", "lower"),
    "vt_throughput_mb_s": ("cpu_ms", "higher"),
}
NOISE_SPREAD = 3.0  # max/min of normalized samples above this: too noisy to fail on
MIN_SAMPLES = 3


def calibrate():
    def best(fn, n=3):
        out = []
        for _ in range(n):
            t = time.perf_counter()
            fn()
            out.append((time.perf_counter() - t) * 1000)
        return min(out)

    buf = os.urandom(1 << 20) * 48

    def cpu():
        hashlib.sha256(buf).digest()
        hashlib.sha512(buf).digest()

    def spawn():
        for _ in range(60):
            subprocess.run(["true"], check=False)

    return {"cpu_ms": round(best(cpu), 3), "spawn_ms": round(best(spawn), 3)}


def plat():
    return f"{platform.system().lower()}-{platform.machine().lower()}"


def read_samples(d):
    samples = []
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name)
        if not os.path.isdir(p):
            continue
        try:
            cal = json.load(open(os.path.join(p, "cal.json")))
        except (OSError, ValueError):
            continue
        vals = {}
        try:
            for line in open(os.path.join(p, "timing.tsv")):
                k, _, v = line.strip().partition("\t")
                if k and v:
                    vals[k] = float(v)
        except OSError:
            pass
        try:
            vals["vt_throughput_mb_s"] = float(open(os.path.join(p, "vt.txt")).read().strip())
        except (OSError, ValueError):
            pass
        samples.append((cal, vals))
    return samples


def normalize(metric, value, cal):
    calibrator, better = METRICS[metric]
    c = cal.get(calibrator)
    if not c or value <= 0:
        return None
    # lower-better: time per unit of machine speed; higher-better: throughput scaled by the
    # time the calibration took (a slower machine halves the throughput and doubles the time).
    return value / c if better == "lower" else value * c


def summarize(samples):
    out = {}
    for m in METRICS:
        xs = [n for cal, vals in samples if m in vals and (n := normalize(m, vals[m], cal)) is not None]
        if xs:
            out[m] = xs
    return out


def compare(a):
    samples = read_samples(a.samples)
    summary = summarize(samples)
    if len(samples) < MIN_SAMPLES or not summary:
        print(f"perf: {len(samples)} usable sample(s), need {MIN_SAMPLES}; nothing to gate")
        return 0
    medians = {m: statistics.median(xs) for m, xs in summary.items()}
    if a.candidate:
        with open(a.candidate, "w") as f:
            json.dump({"platforms": {plat(): {m: round(v, 6) for m, v in medians.items()}}}, f, indent=2)
            f.write("\n")
    base_doc = {}
    try:
        base_doc = json.load(open(a.baseline))
    except (OSError, ValueError):
        pass
    if a.update:
        base_doc.setdefault("platforms", {})[plat()] = {m: round(v, 6) for m, v in medians.items()}
        base_doc.setdefault("tolerance", a.tolerance if a.tolerance is not None else 1.0)
        with open(a.baseline, "w") as f:
            json.dump(base_doc, f, indent=2, sort_keys=True)
            f.write("\n")
        print(f"perf: baseline for {plat()} written to {a.baseline}")
        return 0
    base = base_doc.get("platforms", {}).get(plat())
    tol = a.tolerance if a.tolerance is not None else (base_doc.get("tolerance") or 1.0)
    print(f"perf: platform {plat()}, {len(samples)} samples, tolerance +{tol * 100:.0f}%"
          + ("" if base else "  (no baseline for this platform: report only)"))
    print(f"  {'metric':28} {'median':>10} {'baseline':>10} {'change':>8}  verdict")
    failed = 0
    for m, xs in summary.items():
        med = medians[m]
        spread = max(xs) / min(xs) if min(xs) > 0 else float("inf")
        b = base.get(m) if base else None
        if b is None:
            print(f"  {m:28} {med:10.4f} {'-':>10} {'-':>8}  REPORT")
            continue
        better = METRICS[m][1]
        change = (med / b - 1) if better == "lower" else (b / med - 1)  # positive = worse
        bad = change > tol
        if bad and spread > NOISE_SPREAD:
            verdict = f"NOISY (spread {spread:.1f}x), not failed"
        elif bad:
            verdict = "REGRESSION"
            failed += 1
        else:
            verdict = "ok"
        print(f"  {m:28} {med:10.4f} {b:10.4f} {change * 100:+7.0f}%  {verdict}")
    if failed:
        print(f"perf: {failed} regression(s) beyond +{tol * 100:.0f}% of the baseline ratio")
        return 1
    return 0


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("calibrate")
    c = sub.add_parser("compare")
    c.add_argument("samples")
    c.add_argument("--baseline", default="tests/perf/baseline.json")
    c.add_argument("--tolerance", type=float, default=None)
    c.add_argument("--update", action="store_true")
    c.add_argument("--candidate")
    a = ap.parse_args(argv)
    if a.cmd == "calibrate":
        print(json.dumps(calibrate()))
        return 0
    if not os.path.isdir(a.samples):
        print(f"perf: {a.samples}: no such directory", file=sys.stderr)
        return 2
    return compare(a)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
