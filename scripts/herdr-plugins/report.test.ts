import { describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { build, collect, Invalid, LIMITS, md, render, validate } from "../herdr-plugins-report";

function result(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    schema: "vibeke.herdr-plugin-result/1",
    plugin_id: "owner.projects",
    repository: "owner/herdr-projects",
    rank: 3,
    os: "linux",
    commit: "0123456789abcdef0123456789abcdef01234567",
    dependency_mode: "unpinned",
    reproducible: false,
    outcome: "passed",
    fails_run: false,
    surfaces: ["action", "pane:popup", "startup"],
    coverage: { ok: true, missing: [], skipped: [] },
    lifecycle: [{ check: "install", outcome: "passed", reason: null }],
    steps: [
      {
        id: "open-projects",
        kind: "action",
        outcome: "passed",
        vibeke: "passed",
        herdr: "not_run",
        known: null,
        unexpected_pass: false,
        reason: null,
        evidence: ["invocation completed"],
        grids: [
          {
            pane: "projects",
            vibeke: "grids/open-projects.vibeke.json",
            herdr: null,
            image: "grids/open-projects.svg",
            diff_cells: null,
            placement_matches: null,
          },
        ],
        duration_ms: 1200,
      },
    ],
    duration_ms: 5000,
    ...over,
  };
}

function step(over: Record<string, unknown>): Record<string, unknown> {
  return { ...(result().steps as Record<string, unknown>[])[0], ...over };
}

function rejects(v: unknown, why: RegExp) {
  try {
    validate(v);
  } catch (e) {
    expect(e).toBeInstanceOf(Invalid);
    expect((e as Error).message).toMatch(why);
    return;
  }
  throw new Error("accepted invalid input");
}

describe("validate", () => {
  test("accepts a valid result", () => {
    const r = validate(result());
    expect(r.plugin_id).toBe("owner.projects");
    expect(r.steps[0].grids[0].image).toBe("grids/open-projects.svg");
  });

  test("accepts absent optional fields", () => {
    const s = step({});
    delete s.known;
    delete s.reason;
    delete s.grids;
    expect(validate(result({ steps: [s] })).steps[0].grids).toEqual([]);
  });

  test("rejects a non-object", () => rejects([], /not an object/));
  test("rejects unknown fields", () => rejects(result({ extra: 1 }), /\$\.extra: unknown field/));
  test("rejects unknown nested fields", () =>
    rejects(result({ steps: [step({ html: "<b>" })] }), /\$\.steps\[0\]\.html: unknown field/));
  test("rejects missing fields", () => {
    const r = result();
    delete r.outcome;
    rejects(r, /\$\.outcome: missing field/);
  });
  test("rejects another schema", () => rejects(result({ schema: "vibeke.herdr-plugin-result/2" }), /unknown schema/));
  test("rejects unknown outcomes", () => rejects(result({ outcome: "fine" }), /not an allowed value/));
  test("rejects unknown side results", () =>
    rejects(result({ steps: [step({ herdr: "skipped" })] }), /\$\.steps\[0\]\.herdr: not an allowed value/));
  test("rejects unknown platforms", () => rejects(result({ os: "windows" }), /\$\.os/));
  test("rejects wrong types", () => {
    rejects(result({ fails_run: "no" }), /not a boolean/);
    rejects(result({ rank: "1" }), /not a non-negative integer/);
    rejects(result({ rank: -1 }), /not a non-negative integer/);
    rejects(result({ duration_ms: 1.5 }), /not a non-negative integer/);
    rejects(result({ surfaces: "action" }), /not an array/);
    rejects(result({ plugin_id: 7 }), /not a string/);
  });
  test("rejects bad identifiers", () => {
    rejects(result({ commit: "main" }), /\$\.commit: has an invalid format/);
    rejects(result({ repository: "../x" }), /\$\.repository: has an invalid format/);
    rejects(result({ plugin_id: "a b" }), /\$\.plugin_id: has an invalid format/);
  });
  test("rejects long strings", () =>
    rejects(result({ steps: [step({ reason: "x".repeat(LIMITS.text + 1) })] }), /longer than/));
  test("rejects long lists", () =>
    rejects(result({ surfaces: Array(LIMITS.listItems + 1).fill("action") }), /more than/));
  test("rejects control and bidi characters", () => {
    rejects(result({ steps: [step({ evidence: ["\u001b[31mred"] })] }), /control characters/);
    rejects(result({ steps: [step({ reason: "a\u202eb" })] }), /control characters/);
  });
  test("rejects unsafe grid paths", () => {
    for (const p of ["/etc/passwd", "../x.svg", "grids/../../x", "grids//x", "./x", "a\\b", "file:x", "x y"]) {
      const g = { pane: "p", image: p, placement_matches: null };
      rejects(result({ steps: [step({ grids: [g] })] }), /\.image/);
    }
  });
  test("rejects a reproducible unpinned result", () =>
    rejects(result({ reproducible: true }), /cannot be reproducible/));
});

describe("md", () => {
  test("escapes table, code, link and HTML syntax", () => {
    const out = md("a|b `c` [x](javascript:y) <img src=x> & *b* _i_ ~s~ \\");
    expect(out).not.toMatch(/(^|[^\\])[|`[\]()*_~]/);
    expect(out).not.toContain("<");
    expect(out).toContain("&lt;img src\\=x&gt;");
    expect(out).toContain("&amp;");
    expect(out).toContain("\\|");
    expect(out).toContain("\\`");
  });
  test("removes line breaks", () => {
    expect(md("a\n| b |\r\n# h")).not.toMatch(/[\r\n]/);
  });
  test("breaks mentions, references and autolinks", () => {
    const out = md("@octocat owner/repo#12 https://evil.example www.evil.example");
    expect(out).not.toContain("@o");
    expect(out).not.toContain("#1");
    expect(out).not.toContain("://");
    expect(out).not.toMatch(/www\\\./);
  });
  test("replaces control characters", () => {
    expect(md("a\u0007b\u202ec")).toBe("a\ufffdb\ufffdc");
  });
});

describe("build and render", () => {
  const ok = { file: "a/owner__herdr-projects-linux.json", text: JSON.stringify(result()) };

  test("one row per plugin and platform, sorted by rank", () => {
    const other = {
      file: "b/x__y-macos.json",
      text: JSON.stringify(result({ plugin_id: "x.y", repository: "x/y", rank: 1, os: "macos" })),
    };
    const m = build([ok, other]);
    expect(m.rows.map((r) => r.plugin_id)).toEqual(["x.y", "owner.projects"]);
    expect(m.failing).toBe(false);
    expect(m.rows[1].reproducible).toBe(false);
    expect(m.rows[1].steps).toEqual({ passed: 1 });
    const text = render(m);
    expect(text).toContain("non\\-reproducible");
    expect(text).toContain("| 3 | owner\\.projects (owner\\/herdr\\-projects) | linux | passed |");
  });

  test("failing outcomes and flags fail the run", () => {
    for (const over of [{ outcome: "compat_failure" }, { outcome: "harness_error" }, { fails_run: true }]) {
      expect(build([{ file: "a-linux.json", text: JSON.stringify(result(over)) }]).failing).toBe(true);
    }
    for (const over of [{ outcome: "dependency_failure" }, { outcome: "suspected_compat_failure" }]) {
      expect(build([{ file: "a-linux.json", text: JSON.stringify(result(over)) }]).failing).toBe(false);
    }
  });

  test("lists unexpected passes, coverage failures and partial surfaces", () => {
    const r = result({
      fails_run: true,
      coverage: { ok: false, missing: ["event:tab.created"], skipped: [] },
      steps: [
        step({ unexpected_pass: true, known: "suspected_compat_failure" }),
        step({ id: "hook", kind: "event", outcome: "not_exercised", reason: "agent CLI missing" }),
        step({ id: "link", kind: "link", outcome: "compat_failure", vibeke: "failed", herdr: "passed", reason: "x" }),
      ],
    });
    const m = build([{ file: "p-linux.json", text: JSON.stringify(r) }]);
    expect(m.unexpected_passes).toEqual([
      { plugin_id: "owner.projects", os: "linux", step: "open-projects", known: "suspected_compat_failure" },
    ]);
    expect(m.coverage_failures[0].missing).toEqual(["event:tab.created"]);
    expect(m.rows[0].failed_surfaces).toEqual(["link"]);
    expect(m.rows[0].partial_surfaces).toEqual(["event", "event:tab.created"]);
    const text = render(m);
    expect(text).toContain("Unexpected pass: owner\\.projects on linux, step open\\-projects");
    expect(text).toContain("Coverage failure");
  });

  test("rejects bad JSON, mismatched names and duplicates, and reports missing results", () => {
    const m = build(
      [
        ok,
        { file: "dup/owner__herdr-projects-linux.json", text: JSON.stringify(result()) },
        { file: "bad-linux.json", text: "{" },
        { file: "wrong-macos.json", text: JSON.stringify(result({ plugin_id: "z.z" })) },
        { file: "big-linux.json", error: "larger than 1 bytes" },
      ],
      [
        { plugin_id: "owner.projects", os: "linux" },
        { plugin_id: "owner.projects", os: "macos" },
      ],
    );
    expect(m.rejected.map((r) => r.reason)).toEqual([
      "duplicate result for this plugin and platform",
      "not valid JSON",
      "$.os: does not match the file name",
      "larger than 1 bytes",
    ]);
    expect(m.missing).toEqual([{ plugin_id: "owner.projects", os: "macos" }]);
    expect(m.failing).toBe(true);
    expect(render(m)).toContain("## Rejected result files");
  });

  test("escapes hostile text in the rendered Markdown", () => {
    const r = result({
      outcome: "not_exercised",
      coverage: { ok: false, missing: ["<script>|`x`"], skipped: [] },
    });
    const text = render(build([{ file: "p-linux.json", text: JSON.stringify(r) }]));
    expect(text).not.toContain("<script>");
    expect(text).toContain("&lt;script&gt;\\|\\`x\\`");
  });
});

describe("collect", () => {
  test("reads only regular result files, one directory deep, with size limits", () => {
    const dir = mkdtempSync(join(tmpdir(), "hp-report-"));
    mkdirSync(join(dir, "artifact-a", "grids"), { recursive: true });
    mkdirSync(join(dir, "artifact-a", "deep"), { recursive: true });
    writeFileSync(join(dir, "artifact-a", "owner__repo-linux.json"), JSON.stringify(result()));
    writeFileSync(join(dir, "artifact-a", "grids", "x-linux.json"), "{}");
    writeFileSync(join(dir, "artifact-a", "notes.txt"), "hi");
    writeFileSync(join(dir, "artifact-a", "huge-macos.json"), "x".repeat(LIMITS.fileBytes + 1));
    symlinkSync("/etc/hosts", join(dir, "artifact-a", "link-linux.json"));
    const inputs = collect(dir);
    expect(inputs.map((i) => [i.file, "error" in i ? i.error : "ok"])).toEqual([
      ["artifact-a/huge-macos.json", `larger than ${LIMITS.fileBytes} bytes`],
      ["artifact-a/link-linux.json", "not a regular file"],
      ["artifact-a/owner__repo-linux.json", "ok"],
    ]);
  });
});
