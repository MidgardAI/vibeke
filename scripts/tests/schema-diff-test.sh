#!/bin/sh
# Self-test for scripts/schema-diff.py: additions pass, each kind of breaking change fails,
# and the real in-tree schema is compatible with itself and its freeze. Fast, no cargo.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/schema-diff-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
tool="$root/scripts/schema-diff.py"

python3 - "$tmp" <<'EOF'
import json, sys, copy
d = sys.argv[1]
base = {
  "$defs": {"T": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
  "x-methods": {
    "a.get": {"mutating": False, "scope": "pane", "pane_scope": "open",
      "params": {"type": "object", "properties": {"t": {"$ref": "#/$defs/T"}, "n": {"type": "integer"}, "mode": {"enum": ["x", "y"]}}, "required": ["t"]},
      "result": {"type": "object", "properties": {"v": {"type": "string"}, "w": {"anyOf": [{"type": "string"}, {"type": "null"}]}}, "required": ["v"]}},
    "a.set": {"mutating": True, "scope": "pane", "pane_scope": "own_target", "params": {"type": "object"}, "result": {"type": "object"}},
  },
  "x-events": {"a.changed": {"subject": {"type": "object"}, "data": {"type": "object", "properties": {"k": {"type": "string"}}, "required": ["k"]}}},
}
frozen = {"methods": {"a.get": {"mutating": False, "scope": "pane", "pane_scope": "open"}}}
def w(name, v):
    json.dump(v, open(f"{d}/{name}.json", "w"))
w("base", base); w("frozen", frozen)
ok = copy.deepcopy(base)
m = ok["x-methods"]["a.get"]
m["params"]["properties"]["extra"] = {"type": "string"}          # new optional param
m["result"]["properties"]["more"] = {"type": "integer"}           # new result field
ok["x-methods"]["b.new"] = {"mutating": False, "scope": "session", "pane_scope": "forbidden", "params": {}, "result": {}}
ok["x-events"]["b.event"] = {"subject": {}, "data": {}}
w("ok", ok)
def mutate(name, fn):
    v = copy.deepcopy(base); fn(v); w(name, v)
mutate("removed_method", lambda v: v["x-methods"].pop("a.get"))
mutate("flag", lambda v: v["x-methods"]["a.set"].update(mutating=False))
mutate("param_removed", lambda v: v["x-methods"]["a.get"]["params"]["properties"].pop("n"))
mutate("new_required", lambda v: v["x-methods"]["a.get"]["params"]["required"].append("n"))
mutate("result_removed", lambda v: v["x-methods"]["a.get"]["result"]["properties"].pop("v"))
mutate("result_optional", lambda v: v["x-methods"]["a.get"]["result"].update(required=[]))
mutate("type_changed", lambda v: v["x-methods"]["a.get"]["result"]["properties"].update(v={"type": "integer"}))
mutate("enum_removed", lambda v: v["x-methods"]["a.get"]["params"]["properties"]["mode"].update(enum=["x"]))
mutate("ref_field_removed", lambda v: v["$defs"]["T"]["properties"].pop("id"))
mutate("event_removed", lambda v: v["x-events"].pop("a.changed"))
mutate("event_field", lambda v: v["x-events"]["a.changed"]["data"]["properties"].pop("k"))
EOF

check() { # name expected-exit
  set +e
  python3 "$tool" --base-schema "$tmp/base.json" --schema "$tmp/$1.json" --frozen "$tmp/frozen.json" >"$tmp/out.txt" 2>&1
  rc=$?
  set -e
  if [ "$rc" != "$2" ]; then
    echo "FAIL: $1 expected exit $2, got $rc"; cat "$tmp/out.txt"; exit 1
  fi
}
check base 0
check ok 0
for c in removed_method flag param_removed new_required result_removed result_optional \
         type_changed enum_removed ref_field_removed event_removed event_field; do
  check "$c" 1
done

# The real schema against itself, and against its freeze.
python3 "$tool" --base-schema "$root/docs/api/vibeke-1.schema.json" \
  --schema "$root/docs/api/vibeke-1.schema.json" \
  --frozen "$root/docs/api/vibeke-1.frozen.json" >/dev/null
echo "schema-diff-test: ok"
