#!/usr/bin/env python3
"""Breaking-change check for the `vibeke/1` API (spec 10 section 8.2 item 5, spec 07 1.5).

Within a major version only additions are allowed: new methods, new optional params, new
result fields, new event types. This script compares the API schema of a *base* revision (the
last frozen/released one) with the current one and exits 1 listing every breaking change:

  * a frozen method disappeared, or its mutating / scope / pane_scope flag changed;
  * a params shape stopped accepting something it accepted (property removed, type changed,
    enum value removed, a newly required property);
  * a result shape stopped producing something it produced (property removed, type changed,
    a previously always-present property became optional);
  * an event type disappeared, or its subject/data shape narrowed like a result.

It also checks the in-tree freeze (`--frozen`) against the current schema, which is the same
rule the `api_docs` test enforces, so the script works on its own in CI.

  scripts/schema-diff.py --frozen docs/api/vibeke-1.frozen.json --schema docs/api/vibeke-1.schema.json
  scripts/schema-diff.py --base-schema OLD.schema.json --base-frozen OLD.frozen.json \\
      --frozen docs/api/vibeke-1.frozen.json --schema docs/api/vibeke-1.schema.json
  scripts/schema-diff.py --base-ref origin/main      # reads the base files with `git show`

Exit codes: 0 compatible, 1 breaking changes found, 2 usage or input error.
"""
import argparse
import json
import subprocess
import sys

FLAGS = ("mutating", "scope", "pane_scope")


def load(path):
    with open(path) as f:
        return json.load(f)


def git_show(ref, path):
    out = subprocess.run(
        ["git", "show", f"{ref}:{path}"], capture_output=True, text=True
    )
    if out.returncode != 0:
        return None
    return json.loads(out.stdout)


class Cmp:
    def __init__(self, old_defs, new_defs):
        self.od, self.nd = old_defs, new_defs
        self.problems = []
        self.seen = set()

    def resolve(self, s, defs):
        for _ in range(32):
            if isinstance(s, dict) and "$ref" in s:
                name = s["$ref"].rsplit("/", 1)[-1]
                s = defs.get(name, {})
            else:
                break
        return s

    def arms(self, s, defs):
        s = self.resolve(s, defs)
        if isinstance(s, dict):
            for k in ("anyOf", "oneOf"):
                if k in s:
                    out = []
                    for a in s[k]:
                        out.extend(self.arms(a, defs))
                    return out
        return [s]

    @staticmethod
    def types(s):
        if not isinstance(s, dict):
            return None
        t = s.get("type")
        if t is None:
            return None
        return set(t) if isinstance(t, list) else {t}

    def compare(self, old, new, mode, path):
        """`mode` is "in" (params: new must accept all old accepted) or "out" (results)."""
        key = (json.dumps(old, sort_keys=True), json.dumps(new, sort_keys=True), mode)
        if key in self.seen:
            return
        self.seen.add(key)
        o_arms = self.arms(old, self.od)
        n_arms = self.arms(new, self.nd)
        # Every old shape must still have a compatible new arm.
        for oa in o_arms:
            trial = []
            for na in n_arms:
                sub = Cmp(self.od, self.nd)
                sub.seen = self.seen
                sub.arm(oa, na, mode, path)
                if not sub.problems:
                    trial = None
                    break
                trial.append(sub.problems)
            if trial is not None:
                # Report the closest arm's problems (fewest), or a plain removal.
                self.problems.extend(min(trial, key=len) if trial else [f"{path}: shape removed"])

    def arm(self, o, n, mode, path):
        if not isinstance(o, dict) or not isinstance(n, dict):
            return
        ot, nt = self.types(o), self.types(n)
        if ot and nt and ot != nt and not (mode == "in" and ot <= {"integer"} and nt >= {"number"}):
            self.problems.append(f"{path}: type {sorted(ot)} became {sorted(nt)}")
            return
        if "enum" in o and mode == "in":
            if "enum" in n:
                for v in o["enum"]:
                    if v not in n["enum"]:
                        self.problems.append(f"{path}: enum value {v!r} removed")
        if "const" in o and "const" in n and o["const"] != n["const"]:
            self.problems.append(f"{path}: const {o['const']!r} became {n['const']!r}")
        op, np_ = o.get("properties"), n.get("properties")
        if op is not None:
            np_ = np_ or {}
            for name, osub in op.items():
                if name not in np_:
                    self.problems.append(f"{path}.{name}: property removed")
                else:
                    self.compare(osub, np_[name], mode, f"{path}.{name}")
            orq, nrq = set(o.get("required", [])), set(n.get("required", []))
            if mode == "in":
                for name in sorted(nrq - orq):
                    self.problems.append(f"{path}.{name}: newly required parameter")
            else:
                for name in sorted(orq - nrq):
                    if name in np_:
                        self.problems.append(f"{path}.{name}: was always present, now optional")
        if "items" in o and "items" in n:
            self.compare(o["items"], n["items"], mode, f"{path}[]")


def diff(base_schema, cur_schema, base_frozen=None, frozen=None):
    problems = []
    bm, cm = base_schema.get("x-methods", {}), cur_schema.get("x-methods", {})
    bd, cd = base_schema.get("$defs", {}), cur_schema.get("$defs", {})
    for name, om in sorted(bm.items()):
        nm = cm.get(name)
        if nm is None:
            problems.append(f"method {name}: removed")
            continue
        for flag in FLAGS:
            if om.get(flag) != nm.get(flag):
                problems.append(f"method {name}: {flag} changed {om.get(flag)!r} -> {nm.get(flag)!r}")
        for part, mode in (("params", "in"), ("result", "out")):
            c = Cmp(bd, cd)
            c.compare(om.get(part, {}), nm.get(part, {}), mode, f"{name}.{part}")
            problems.extend(c.problems)
    be, ce = base_schema.get("x-events", {}), cur_schema.get("x-events", {})
    for name, oe in sorted(be.items()):
        ne = ce.get(name)
        if ne is None:
            problems.append(f"event {name}: removed")
            continue
        for part in ("subject", "data"):
            c = Cmp(bd, cd)
            c.compare(oe.get(part, {}), ne.get(part, {}), "out", f"event {name}.{part}")
            problems.extend(c.problems)
    if base_frozen and frozen:
        problems.extend(check_frozen(base_frozen, cur_schema))
    return problems


def check_frozen(frozen, schema):
    """A freeze file against a schema: no frozen method removed, no flag changed."""
    problems = []
    methods = schema.get("x-methods", {})
    for name, f in sorted(frozen.get("methods", {}).items()):
        m = methods.get(name)
        if m is None:
            problems.append(f"frozen method {name}: removed")
            continue
        for flag in FLAGS:
            if flag in f and f[flag] != m.get(flag):
                problems.append(f"frozen method {name}: {flag} {f[flag]!r} -> {m.get(flag)!r}")
    return problems


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--schema", default="docs/api/vibeke-1.schema.json")
    ap.add_argument("--frozen", default="docs/api/vibeke-1.frozen.json")
    ap.add_argument("--base-schema")
    ap.add_argument("--base-frozen")
    ap.add_argument("--base-ref", help="git revision to read the base schema and freeze from")
    a = ap.parse_args(argv)
    try:
        cur = load(a.schema)
        frozen = load(a.frozen)
        base_schema = load(a.base_schema) if a.base_schema else None
        base_frozen = load(a.base_frozen) if a.base_frozen else None
        if a.base_ref:
            base_schema = git_show(a.base_ref, "docs/api/vibeke-1.schema.json")
            base_frozen = git_show(a.base_ref, "docs/api/vibeke-1.frozen.json")
            if base_schema is None:
                print(f"schema-diff: {a.base_ref} has no docs/api/vibeke-1.schema.json; "
                      "only the in-tree freeze is checked")
    except (OSError, ValueError) as e:
        print(f"schema-diff: {e}", file=sys.stderr)
        return 2

    problems = check_frozen(frozen, cur)
    if base_frozen:
        problems += [f"vs base freeze: {p}" for p in check_frozen(base_frozen, cur)]
    if base_schema:
        problems += diff(base_schema, cur)
    # De-duplicate, keep order.
    seen, out = set(), []
    for p in problems:
        if p not in seen:
            seen.add(p)
            out.append(p)
    if out:
        print(f"schema-diff: {len(out)} breaking change(s) to vibeke/1:")
        for p in out:
            print(f"  - {p}")
        print("Within a major version only additions are allowed (spec 07 1.5). If this is an "
              "intended pre-1.0 change, update the freeze with VIBEKE_UPDATE_API_FREEZE=1.")
        return 1
    print("schema-diff: no breaking changes"
          + (" against the base" if base_schema else " (in-tree freeze only)"))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
