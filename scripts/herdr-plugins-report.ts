#!/usr/bin/env bun
// Builds the real Herdr plugin matrix (spec 18 §12) from the per-plugin result files.
//
//   bun scripts/herdr-plugins-report.ts --in <dir> --out <dir> [--expect <matrix.json>]
//
// The result files come from jobs that ran third-party plugin code, so this script treats them
// as hostile data (18 §5.6): it limits file sizes, accepts only regular files, validates the
// closed `vibeke.herdr-plugin-result/1` schema (crates/vk-conformance/src/result.rs), escapes
// every string it puts into Markdown, and never passes HTML through. It never runs or loads
// anything from the inputs.
//
// It writes `matrix.json` and `matrix.md` to the output directory, then exits 1 when a result
// fails the run, has the outcome `compat_failure` or `harness_error`, is rejected, or is missing.

import { lstatSync, mkdirSync, openSync, readSync, closeSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const SCHEMA = "vibeke.herdr-plugin-result/1";
export const MATRIX_SCHEMA = "vibeke.herdr-plugin-matrix/1";

export const LIMITS = {
  fileBytes: 1024 * 1024,
  totalBytes: 32 * 1024 * 1024,
  files: 400,
  id: 200,
  text: 2000,
  path: 256,
  listItems: 200,
  steps: 500,
  grids: 32,
};

export const OUTCOMES = [
  "passed",
  "not_exercised",
  "dependency_failure",
  "suspected_compat_failure",
  "compat_failure",
  "harness_error",
] as const;
export type Outcome = (typeof OUTCOMES)[number];
export const SIDES = ["passed", "failed", "not_run", "harness_error"] as const;
export type Side = (typeof SIDES)[number];

export type Grid = {
  pane: string;
  vibeke: string | null;
  herdr: string | null;
  image: string | null;
  diff_cells: number | null;
  placement_matches: boolean | null;
};
export type Step = {
  id: string;
  kind: string;
  outcome: Outcome;
  vibeke: Side;
  herdr: Side;
  known: Outcome | null;
  unexpected_pass: boolean;
  reason: string | null;
  evidence: string[];
  grids: Grid[];
  duration_ms: number;
};
export type Check = { check: string; outcome: Outcome; reason: string | null };
export type PluginResult = {
  schema: string;
  plugin_id: string;
  repository: string;
  rank: number;
  os: "linux" | "macos";
  commit: string;
  dependency_mode: "verified" | "unpinned";
  reproducible: boolean;
  outcome: Outcome;
  fails_run: boolean;
  surfaces: string[];
  coverage: { ok: boolean; missing: string[]; skipped: string[] };
  lifecycle: Check[];
  steps: Step[];
  duration_ms: number;
};

/** A validation failure; `path` is the JSON path of the offending value. */
export class Invalid extends Error {
  constructor(
    public path: string,
    public why: string,
  ) {
    super(`${path}: ${why}`);
  }
}

type Obj = Record<string, unknown>;

function object(v: unknown, path: string, required: string[], optional: string[] = []): Obj {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Invalid(path, "not an object");
  const o = v as Obj;
  const allowed = new Set([...required, ...optional]);
  for (const k of Object.keys(o)) {
    if (!allowed.has(k)) throw new Invalid(`${path}.${short(k)}`, "unknown field");
  }
  for (const k of required) {
    if (!(k in o)) throw new Invalid(`${path}.${k}`, "missing field");
  }
  return o;
}

function short(s: string): string {
  return s.length > 40 ? `${s.slice(0, 40)}…` : s;
}

// C0 controls other than tab and newline, DEL, C1 controls, and bidirectional overrides.
const FORBIDDEN = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/;
const FORBIDDEN_ALL = new RegExp(FORBIDDEN.source, "g");

function str(v: unknown, path: string, max: number, pattern?: RegExp): string {
  if (typeof v !== "string") throw new Invalid(path, "not a string");
  if (v.length > max) throw new Invalid(path, `longer than ${max} characters`);
  if (FORBIDDEN.test(v)) throw new Invalid(path, "contains control characters");
  if (pattern && !pattern.test(v)) throw new Invalid(path, "has an invalid format");
  return v;
}

function optStr(v: unknown, path: string, max: number): string | null {
  return v === undefined || v === null ? null : str(v, path, max);
}

function bool(v: unknown, path: string): boolean {
  if (typeof v !== "boolean") throw new Invalid(path, "not a boolean");
  return v;
}

function uint(v: unknown, path: string, max = Number.MAX_SAFE_INTEGER): number {
  if (typeof v !== "number" || !Number.isInteger(v) || v < 0 || v > max) {
    throw new Invalid(path, "not a non-negative integer in range");
  }
  return v;
}

function oneOf<T extends string>(v: unknown, path: string, values: readonly T[]): T {
  if (typeof v !== "string" || !(values as readonly string[]).includes(v)) {
    throw new Invalid(path, "not an allowed value");
  }
  return v as T;
}

function list<T>(v: unknown, path: string, max: number, item: (x: unknown, p: string) => T): T[] {
  if (!Array.isArray(v)) throw new Invalid(path, "not an array");
  if (v.length > max) throw new Invalid(path, `more than ${max} items`);
  return v.map((x, i) => item(x, `${path}[${i}]`));
}

const ID = /^[A-Za-z0-9][A-Za-z0-9_.:\/-]*$/;
const PLUGIN_ID = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/;
const REPOSITORY = /^[A-Za-z0-9][A-Za-z0-9-]*\/(?!\.\.?$)[A-Za-z0-9_.-]+$/;
const COMMIT = /^[0-9a-f]{40}$/;

/** A relative path inside the result directory: no absolute paths, `..`, or odd characters. */
export function relPath(v: unknown, path: string): string {
  const s = str(v, path, LIMITS.path, /^[A-Za-z0-9_.\/-]+$/);
  if (s.startsWith("/")) throw new Invalid(path, "absolute path");
  const parts = s.split("/");
  if (parts.some((p) => p === "" || p === "." || p === "..")) {
    throw new Invalid(path, "path has empty, '.' or '..' segments");
  }
  return s;
}

function optPath(v: unknown, path: string): string | null {
  return v === undefined || v === null ? null : relPath(v, path);
}

function grid(v: unknown, p: string): Grid {
  const o = object(v, p, ["pane", "placement_matches"], ["vibeke", "herdr", "image", "diff_cells"]);
  return {
    pane: str(o.pane, `${p}.pane`, LIMITS.id, ID),
    vibeke: optPath(o.vibeke, `${p}.vibeke`),
    herdr: optPath(o.herdr, `${p}.herdr`),
    image: optPath(o.image, `${p}.image`),
    diff_cells: o.diff_cells === undefined || o.diff_cells === null ? null : uint(o.diff_cells, `${p}.diff_cells`, 0xffffffff),
    placement_matches: o.placement_matches === null ? null : bool(o.placement_matches, `${p}.placement_matches`),
  };
}

function step(v: unknown, p: string): Step {
  const o = object(
    v,
    p,
    ["id", "kind", "outcome", "vibeke", "herdr", "unexpected_pass", "evidence", "duration_ms"],
    ["known", "reason", "grids"],
  );
  return {
    id: str(o.id, `${p}.id`, LIMITS.id, ID),
    kind: str(o.kind, `${p}.kind`, LIMITS.id, ID),
    outcome: oneOf(o.outcome, `${p}.outcome`, OUTCOMES),
    vibeke: oneOf(o.vibeke, `${p}.vibeke`, SIDES),
    herdr: oneOf(o.herdr, `${p}.herdr`, SIDES),
    known: o.known === undefined || o.known === null ? null : oneOf(o.known, `${p}.known`, OUTCOMES),
    unexpected_pass: bool(o.unexpected_pass, `${p}.unexpected_pass`),
    reason: optStr(o.reason, `${p}.reason`, LIMITS.text),
    evidence: list(o.evidence, `${p}.evidence`, LIMITS.listItems, (x, q) => str(x, q, LIMITS.text)),
    grids: o.grids === undefined ? [] : list(o.grids, `${p}.grids`, LIMITS.grids, grid),
    duration_ms: uint(o.duration_ms, `${p}.duration_ms`),
  };
}

function check(v: unknown, p: string): Check {
  const o = object(v, p, ["check", "outcome"], ["reason"]);
  return {
    check: str(o.check, `${p}.check`, LIMITS.id, ID),
    outcome: oneOf(o.outcome, `${p}.outcome`, OUTCOMES),
    reason: optStr(o.reason, `${p}.reason`, LIMITS.text),
  };
}

/** Validates one parsed result file against the closed schema. */
export function validate(v: unknown): PluginResult {
  const o = object(v, "$", [
    "schema",
    "plugin_id",
    "repository",
    "rank",
    "os",
    "commit",
    "dependency_mode",
    "reproducible",
    "outcome",
    "fails_run",
    "surfaces",
    "coverage",
    "lifecycle",
    "steps",
    "duration_ms",
  ]);
  if (o.schema !== SCHEMA) throw new Invalid("$.schema", "unknown schema");
  const cov = object(o.coverage, "$.coverage", ["ok", "missing", "skipped"]);
  const r: PluginResult = {
    schema: SCHEMA,
    plugin_id: str(o.plugin_id, "$.plugin_id", LIMITS.id, PLUGIN_ID),
    repository: str(o.repository, "$.repository", LIMITS.id, REPOSITORY),
    rank: uint(o.rank, "$.rank", 1000),
    os: oneOf(o.os, "$.os", ["linux", "macos"] as const),
    commit: str(o.commit, "$.commit", 40, COMMIT),
    dependency_mode: oneOf(o.dependency_mode, "$.dependency_mode", ["verified", "unpinned"] as const),
    reproducible: bool(o.reproducible, "$.reproducible"),
    outcome: oneOf(o.outcome, "$.outcome", OUTCOMES),
    fails_run: bool(o.fails_run, "$.fails_run"),
    surfaces: list(o.surfaces, "$.surfaces", LIMITS.listItems, (x, q) => str(x, q, LIMITS.id, ID)),
    coverage: {
      ok: bool(cov.ok, "$.coverage.ok"),
      missing: list(cov.missing, "$.coverage.missing", LIMITS.listItems, (x, q) => str(x, q, LIMITS.id)),
      skipped: list(cov.skipped, "$.coverage.skipped", LIMITS.listItems, (x, q) => str(x, q, LIMITS.text)),
    },
    lifecycle: list(o.lifecycle, "$.lifecycle", LIMITS.listItems, check),
    steps: list(o.steps, "$.steps", LIMITS.steps, step),
    duration_ms: uint(o.duration_ms, "$.duration_ms"),
  };
  if (r.dependency_mode === "unpinned" && r.reproducible) {
    throw new Invalid("$.reproducible", "an unpinned result cannot be reproducible");
  }
  return r;
}

const ZWSP = "\u200b";

/**
 * Escapes text for a Markdown table cell or paragraph on GitHub. Every ASCII punctuation
 * character gets a backslash, `<`, `>` and `&` become entities, line breaks become spaces, and a
 * zero-width space after `@`, `#` and `://` keeps GitHub from turning text into mentions, issue
 * references or links. Nothing from the input can open HTML, a link, code or a table cell.
 */
export function md(s: string): string {
  let out = "";
  for (const ch of s.replace(FORBIDDEN_ALL, "\ufffd")) {
    if (ch === "<") out += "&lt;";
    else if (ch === ">") out += "&gt;";
    else if (ch === "&") out += "&amp;";
    else if (ch === "\n" || ch === "\r" || ch === "\t") out += " ";
    else if (ch === "@" || ch === "#") out += `\\${ch}${ZWSP}`;
    else if (/[!-\/:-?\[-`{-~]/.test(ch)) out += `\\${ch}`;
    else out += ch;
  }
  // `://` was escaped to `\:\/\/`; break it so extended autolinks never start.
  return out.replaceAll("\\:\\/\\/", `\\:${ZWSP}\\/\\/`).replace(/www\\\./gi, (m) => `${m.slice(0, 3)}${ZWSP}${m.slice(3)}`);
}

export type Row = {
  rank: number;
  plugin_id: string;
  repository: string;
  os: string;
  outcome: Outcome;
  reproducible: boolean;
  dependency_mode: string;
  fails_run: boolean;
  steps: Partial<Record<Outcome, number>>;
  lifecycle: Partial<Record<Outcome, number>>;
  surfaces: string[];
  failed_surfaces: string[];
  partial_surfaces: string[];
  file: string;
};

export type Matrix = {
  schema: string;
  rows: Row[];
  unexpected_passes: { plugin_id: string; os: string; step: string; known: Outcome | null }[];
  coverage_failures: { plugin_id: string; os: string; missing: string[] }[];
  rejected: { file: string; reason: string }[];
  missing: { plugin_id: string; os: string }[];
  failing: boolean;
};

const FAILED: Outcome[] = ["dependency_failure", "suspected_compat_failure", "compat_failure", "harness_error"];

function counts(outcomes: Outcome[]): Partial<Record<Outcome, number>> {
  const c: Partial<Record<Outcome, number>> = {};
  for (const o of outcomes) c[o] = (c[o] ?? 0) + 1;
  return c;
}

export function row(r: PluginResult, file: string): Row {
  const failed = new Set<string>();
  const partial = new Set<string>();
  for (const s of r.steps) {
    if (FAILED.includes(s.outcome)) failed.add(s.kind);
    else if (s.outcome === "not_exercised") partial.add(s.kind);
  }
  for (const m of r.coverage.missing) partial.add(m);
  return {
    rank: r.rank,
    plugin_id: r.plugin_id,
    repository: r.repository,
    os: r.os,
    outcome: r.outcome,
    reproducible: r.reproducible && r.dependency_mode === "verified",
    dependency_mode: r.dependency_mode,
    fails_run: r.fails_run,
    steps: counts(r.steps.map((s) => s.outcome)),
    lifecycle: counts(r.lifecycle.map((c) => c.outcome)),
    surfaces: [...r.surfaces].sort(),
    failed_surfaces: [...failed].sort(),
    partial_surfaces: [...partial].filter((s) => !failed.has(s)).sort(),
    file,
  };
}

export type Input = { file: string; text: string } | { file: string; error: string };

export function build(inputs: Input[], expect: { plugin_id: string; os: string }[] = []): Matrix {
  const m: Matrix = {
    schema: MATRIX_SCHEMA,
    rows: [],
    unexpected_passes: [],
    coverage_failures: [],
    rejected: [],
    missing: [],
    failing: false,
  };
  const seen = new Set<string>();
  for (const input of inputs) {
    if ("error" in input) {
      m.rejected.push({ file: input.file, reason: input.error });
      continue;
    }
    let r: PluginResult;
    try {
      r = validate(JSON.parse(input.text));
      const name = input.file.split("/").pop() ?? "";
      if (!name.endsWith(`-${r.os}.json`)) throw new Invalid("$.os", "does not match the file name");
    } catch (e) {
      const reason = e instanceof Invalid ? e.message : e instanceof SyntaxError ? "not valid JSON" : "unreadable";
      m.rejected.push({ file: input.file, reason });
      continue;
    }
    const key = `${r.plugin_id}\u0000${r.os}`;
    if (seen.has(key)) {
      m.rejected.push({ file: input.file, reason: "duplicate result for this plugin and platform" });
      continue;
    }
    seen.add(key);
    m.rows.push(row(r, input.file));
    for (const s of r.steps) {
      if (s.unexpected_pass) m.unexpected_passes.push({ plugin_id: r.plugin_id, os: r.os, step: s.id, known: s.known });
    }
    if (!r.coverage.ok) m.coverage_failures.push({ plugin_id: r.plugin_id, os: r.os, missing: r.coverage.missing });
  }
  for (const e of expect) {
    if (!seen.has(`${e.plugin_id}\u0000${e.os}`)) m.missing.push({ plugin_id: e.plugin_id, os: e.os });
  }
  m.rows.sort((a, b) => a.rank - b.rank || a.os.localeCompare(b.os) || a.plugin_id.localeCompare(b.plugin_id));
  m.failing =
    m.rejected.length > 0 ||
    m.missing.length > 0 ||
    m.rows.some((r) => r.fails_run || r.outcome === "compat_failure" || r.outcome === "harness_error");
  return m;
}

function fmtCounts(c: Partial<Record<Outcome, number>>): string {
  const parts = OUTCOMES.filter((o) => c[o]).map((o) => `${o} ${c[o]}`);
  return parts.length ? md(parts.join(", ")) : "none";
}

function fmtList(xs: string[]): string {
  return xs.length ? xs.map(md).join(", ") : "none";
}

export function render(m: Matrix): string {
  const lines: string[] = [];
  lines.push("# Real Herdr plugin matrix", "");
  lines.push(
    "Spec 18 regression signal for the highest-ranked eligible Herdr plugins. It is not a compatibility certificate.",
    "",
  );
  lines.push(`Overall: ${m.failing ? "**failing**" : "passing"}.`, "");
  lines.push("| Rank | Plugin | Platform | Outcome | Reproducible | Steps | Lifecycle | Surfaces | Failed surfaces | Partial surfaces |");
  lines.push("|---:|---|---|---|---|---|---|---|---|---|");
  for (const r of m.rows) {
    lines.push(
      `| ${r.rank} | ${md(r.plugin_id)} (${md(r.repository)}) | ${md(r.os)} | ${md(r.outcome)}${r.fails_run ? " (fails run)" : ""} | ${
        r.reproducible ? "yes" : "non\\-reproducible"
      } | ${fmtCounts(r.steps)} | ${fmtCounts(r.lifecycle)} | ${fmtList(r.surfaces)} | ${fmtList(r.failed_surfaces)} | ${fmtList(r.partial_surfaces)} |`,
    );
  }
  if (m.rows.length === 0) lines.push("| | no results | | | | | | | | |");
  lines.push("", "## Unexpected passes and coverage failures", "");
  if (m.unexpected_passes.length === 0 && m.coverage_failures.length === 0) lines.push("None.");
  for (const u of m.unexpected_passes) {
    lines.push(
      `- Unexpected pass: ${md(u.plugin_id)} on ${md(u.os)}, step ${md(u.step)} (known result ${md(u.known ?? "none")}). Update the scenario.`,
    );
  }
  for (const c of m.coverage_failures) {
    lines.push(`- Coverage failure: ${md(c.plugin_id)} on ${md(c.os)}, not covered: ${fmtList(c.missing)}.`);
  }
  if (m.missing.length > 0) {
    lines.push("", "## Missing results", "");
    for (const x of m.missing) lines.push(`- ${md(x.plugin_id)} on ${md(x.os)}: no result file (harness error).`);
  }
  if (m.rejected.length > 0) {
    lines.push("", "## Rejected result files", "");
    for (const x of m.rejected) lines.push(`- ${md(x.file)}: ${md(x.reason)}`);
  }
  return `${lines.join("\n")}\n`;
}

const RESULT_NAME = /^[A-Za-z0-9_.-]{1,200}-(linux|macos)\.json$/;

function readLimited(path: string, max: number): string {
  const fd = openSync(path, "r");
  try {
    const buf = Buffer.alloc(max + 1);
    const n = readSync(fd, buf, 0, max + 1, 0);
    if (n > max) throw new Error("too large");
    return buf.subarray(0, n).toString("utf8");
  } finally {
    closeSync(fd);
  }
}

/**
 * Collects result files from `dir` and its direct subdirectories (one per downloaded artifact).
 * Only regular files whose names look like `<stem>-<os>.json` count; symbolic links and other
 * file types are rejected without being followed.
 */
export function collect(dir: string): Input[] {
  const inputs: Input[] = [];
  let total = 0;
  const visit = (d: string, rel: string, depth: number) => {
    for (const name of readdirSync(d).sort()) {
      const full = join(d, name);
      const relName = rel ? `${rel}/${name}` : name;
      const st = lstatSync(full);
      if (st.isDirectory()) {
        if (depth === 0) visit(full, relName, 1);
        continue;
      }
      if (!RESULT_NAME.test(name)) continue;
      if (inputs.length >= LIMITS.files) {
        inputs.push({ file: relName, error: "too many result files" });
        continue;
      }
      if (!st.isFile()) {
        inputs.push({ file: relName, error: "not a regular file" });
        continue;
      }
      if (st.size > LIMITS.fileBytes) {
        inputs.push({ file: relName, error: `larger than ${LIMITS.fileBytes} bytes` });
        continue;
      }
      if (total + st.size > LIMITS.totalBytes) {
        inputs.push({ file: relName, error: "total size limit reached" });
        continue;
      }
      try {
        const text = readLimited(full, LIMITS.fileBytes);
        total += text.length;
        inputs.push({ file: relName, text });
      } catch {
        inputs.push({ file: relName, error: "unreadable or too large" });
      }
    }
  };
  visit(dir, "", 0);
  return inputs;
}

function arg(args: string[], name: string): string | undefined {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
}

if (import.meta.main) {
  const args = Bun.argv.slice(2);
  const input = arg(args, "--in");
  const out = arg(args, "--out");
  if (!input || !out) {
    console.error("usage: herdr-plugins-report.ts --in <dir> --out <dir> [--expect <matrix.json>]");
    process.exit(2);
  }
  const expectFile = arg(args, "--expect");
  // The expected matrix comes from the trusted plan job, not from the plugin jobs.
  const expect: { plugin_id: string; os: string }[] = expectFile
    ? (JSON.parse(await Bun.file(expectFile).text()).include ?? [])
    : [];
  const m = build(collect(input), expect);
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, "matrix.json"), `${JSON.stringify(m, null, 2)}\n`);
  writeFileSync(join(out, "matrix.md"), render(m));
  console.log(
    `${m.rows.length} results, ${m.rejected.length} rejected, ${m.missing.length} missing: ${m.failing ? "failing" : "passing"}`,
  );
  process.exit(m.failing ? 1 : 0);
}
