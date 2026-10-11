import { describe, expect, test } from "bun:test";
import { build, crossCheck, trustedFrom } from "../herdr-plugins-report";
import { parseLock } from "./lock-tools";

const COMMIT = "0123456789abcdef0123456789abcdef01234567";

const LOCK = `schema = 1

[[plugin]]
rank = 3
repository = "owner/herdr-projects"
repo_id = 1
plugin_id = "owner.projects"
manifest_path = "herdr-plugin.toml"
commit = "${COMMIT}"
manifest_sha256 = "${"a".repeat(64)}"
dependency_mode = "unpinned"
`;

const SCENARIO = `plugin_id = "owner.projects"
repository = "owner/herdr-projects"

[[step]]
id = "open-projects"
kind = "action"

[[step]]
id = "mac-only"
kind = "action"
platforms = ["macos"]

[[step]]
id = "close-projects"
kind = "action"

[[skip]]
surface = "action:doctor"
reason = "needs an account"
`;

const trusted = () => trustedFrom(parseLock(LOCK), new Map([["owner__herdr-projects", SCENARIO]]));

function stepOf(id: string): Record<string, unknown> {
  return {
    id,
    kind: "action",
    outcome: "passed",
    vibeke: "passed",
    herdr: "not_run",
    known: null,
    unexpected_pass: false,
    reason: null,
    evidence: [],
    grids: [],
    duration_ms: 1,
  };
}

function result(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    schema: "vibeke.herdr-plugin-result/1",
    plugin_id: "owner.projects",
    repository: "owner/herdr-projects",
    rank: 3,
    os: "linux",
    commit: COMMIT,
    dependency_mode: "unpinned",
    reproducible: false,
    outcome: "passed",
    fails_run: false,
    surfaces: ["action"],
    coverage: { ok: true, missing: [], skipped: ["action:doctor"] },
    lifecycle: [
      { check: "install", outcome: "passed" },
      { check: "vibeke_side_stopped", outcome: "passed" },
    ],
    steps: [stepOf("open-projects"), stepOf("close-projects")],
    duration_ms: 10,
    ...over,
  };
}

const run = (r: Record<string, unknown>) =>
  build([{ file: "owner__herdr-projects-linux.json", text: JSON.stringify(r) }], [], null, [], trusted());

describe("trusted scenario cross-check", () => {
  test("accepts a correct result and respects step platforms", () => {
    const m = run(result());
    expect(m.rejected).toEqual([]);
    expect(m.failing).toBe(false);
    const mac = trusted().get("owner.projects\u0000macos")!;
    expect(mac.steps).toEqual(["open-projects", "mac-only", "close-projects"]);
  });

  test("rejects a fabricated empty passed result", () => {
    const m = run(result({ steps: [], lifecycle: [], coverage: { ok: true, missing: [], skipped: [] } }));
    expect(m.rows).toEqual([]);
    expect(m.failing).toBe(true);
    expect(m.rejected[0].reason).toContain("harness_error");
    expect(m.rejected[0].reason).toContain("missing scenario steps: open-projects, close-projects");
    expect(m.rejected[0].reason).toContain("lifecycle check install");
  });

  test("rejects a result that misses one scenario step", () => {
    const m = run(result({ steps: [stepOf("open-projects")] }));
    expect(m.failing).toBe(true);
    expect(m.rejected[0].reason).toContain("missing scenario steps: close-projects");
  });

  test("rejects a result with an extra step", () => {
    const m = run(result({ steps: [stepOf("open-projects"), stepOf("close-projects"), stepOf("bonus")] }));
    expect(m.failing).toBe(true);
    expect(m.rejected[0].reason).toContain("steps that are not in the scenario: bonus");
  });

  test("rejects a repeated step, a wrong commit and a wrong skip list", () => {
    const e = trusted().get("owner.projects\u0000linux")!;
    const dup = crossCheck(
      JSON.parse(JSON.stringify(result({ steps: [stepOf("open-projects"), stepOf("open-projects"), stepOf("close-projects")] }))),
      e,
    );
    expect(dup.join()).toContain("duplicate steps");
    expect(run(result({ commit: "f".repeat(40) })).failing).toBe(true);
    expect(run(result({ coverage: { ok: true, missing: [], skipped: [] } })).failing).toBe(true);
  });

  test("rejects a plugin that is not in the lock", () => {
    const m = run(result({ plugin_id: "other.plugin" }));
    expect(m.failing).toBe(true);
    expect(m.rejected[0].reason).toContain("not a locked");
  });
});
