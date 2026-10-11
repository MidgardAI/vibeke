#!/usr/bin/env bun
// Builds the real Herdr plugin matrix (spec 18 §12) from the per-plugin result files.
//
//   bun scripts/herdr-plugins-report.ts --in <dir> --out <dir> [--expect <matrix.json>]
//                                       [--status <dir>] [--lock <top20.lock.toml>] [--scenarios <dir>]
//
// The result files come from jobs that ran third-party plugin code, so this script treats them
// as hostile data (18 §5.6): it limits file sizes, accepts only regular files, validates the
// closed `vibeke.herdr-plugin-result/1` schema (crates/vk-conformance/src/result.rs), escapes
// every string it puts into Markdown, and never passes HTML through. It never runs or loads
// anything from the inputs.
//
// A result file is written inside the plugin's boundary, by a test process that runs as the same
// user as the plugin. The plugin can therefore forge it. This script never trusts its top-level
// verdict: it recomputes the outcome and `fails_run` from the step, lifecycle and coverage
// details, and rejects a result whose top-level fields disagree. It also reads the status files
// (`--status`) that the trusted runner user writes outside the boundary: a plugin whose test
// exited non-zero, or that wrote no result, fails whatever its result file says. A plugin can
// still forge a passing result for a test that exited 0, so the matrix is a regression signal,
// not a security verdict.
//
// With `--lock` (the trusted, checked-in lock), the report also loads the scenario files next to
// it (`--scenarios`, default `<lock dir>/scenarios`) and requires every result to match them: the
// plugin id, OS and commit from the lock, exactly the scenario's steps for that OS, the skipped
// surfaces, and the fixed lifecycle checks. A mismatch is rejected as a `harness_error`. A plugin
// can still forge steps that fit its scenario (spec 18 §5.6).
//
// With `--lock`, the report also lists the deferred plugins
// (`[[defer]]`, 18 §6) with their reasons. They keep their rank but have no plugin job.
//
// It writes `matrix.json` and `matrix.md` to the output directory, then exits 1 when a result
// fails the run, has the outcome `compat_failure` or `harness_error`, is rejected or
// inconsistent, is missing, or its test exit status is non-zero or missing.

import { lstatSync, mkdirSync, openSync, readSync, closeSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { deferral, deferredPlugins, parseLock, stem, supports, type Lock } from "./herdr-plugins/lock-tools";

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

/** The verdict recomputed from a result's details, as the runner computes it (`finish`). */
export type Verdict = { outcome: Outcome; fails_run: boolean };

/** Orders outcomes as crates/vk-conformance/src/outcome.rs does: later is worse. */
export function worst(outcomes: Outcome[]): Outcome {
  let w = 0;
  for (const o of outcomes) w = Math.max(w, OUTCOMES.indexOf(o));
  return OUTCOMES[w];
}

export function verdict(r: PluginResult): Verdict {
  return {
    outcome: worst([...r.lifecycle.map((c) => c.outcome), ...r.steps.map((s) => s.outcome)]),
    fails_run: !r.coverage.ok || r.coverage.missing.length > 0 || r.steps.some((s) => s.unexpected_pass),
  };
}

/** Where the top-level fields of `r` disagree with its details; empty when consistent. */
export function inconsistencies(r: PluginResult, v: Verdict = verdict(r)): string[] {
  const out: string[] = [];
  if (r.outcome !== v.outcome) out.push(`$.outcome is ${r.outcome} but the worst step or lifecycle outcome is ${v.outcome}`);
  if (r.fails_run !== v.fails_run) out.push(`$.fails_run is ${r.fails_run} but the details give ${v.fails_run}`);
  if (r.coverage.ok !== (r.coverage.missing.length === 0)) out.push("$.coverage.ok disagrees with $.coverage.missing");
  return out;
}

/**
 * What the trusted checkout says one result must look like: the identity from the lock and the
 * step ids and skipped surfaces from the scenario, both already filtered for the platform.
 */
export type Expected = {
  plugin_id: string;
  repository: string;
  rank: number;
  os: string;
  commit: string;
  steps: string[];
  skips: string[];
  /** The scenario's known outcome per step on this platform (absent: none). */
  known: Map<string, Outcome>;
};

/** Expected results by `<plugin_id>`, a NUL character, then `<os>`. */
export type Trusted = Map<string, Expected>;

const onPlatform = (platforms: unknown, os: string): boolean =>
  !Array.isArray(platforms) || platforms.length === 0 || platforms.includes(os);

/**
 * Builds the expectations from the trusted lock and the scenario files. `scenarios` maps a
 * scenario stem (`owner__repo`, see `stem`) to the TOML text. Deferred plugins and platforms a
 * plugin does not support get no entry, so a result for them is rejected. Throws when a locked
 * plugin has no scenario: that is a broken checkout, not a plugin failure.
 */
export function trustedFrom(lock: Lock, scenarios: Map<string, string>): Trusted {
  const out: Trusted = new Map();
  for (const p of lock.plugin ?? []) {
    if (deferral(lock, p)) continue;
    const name = stem(lock, p);
    const text = scenarios.get(name);
    if (text === undefined) throw new Error(`no scenario file for ${p.plugin_id} (${name}.toml)`);
    const sc = Bun.TOML.parse(text) as {
      repository?: string;
      step?: { id: string; platforms?: string[]; known?: { outcome: Outcome; platforms?: string[] } }[];
      skip?: { surface: string; platforms?: string[] }[];
    };
    if (sc.repository !== p.repository) throw new Error(`scenario ${name}.toml is for ${sc.repository}, not ${p.repository}`);
    for (const os of ["linux", "macos"] as const) {
      if (!supports(p, os)) continue;
      out.set(`${p.plugin_id}\u0000${os}`, {
        plugin_id: p.plugin_id,
        repository: p.repository,
        rank: p.rank,
        os,
        commit: p.commit,
        steps: (sc.step ?? []).filter((s) => onPlatform(s.platforms, os)).map((s) => s.id),
        known: new Map(
          (sc.step ?? [])
            .filter((s) => onPlatform(s.platforms, os) && s.known && onPlatform(s.known.platforms, os))
            .map((s) => [s.id, s.known!.outcome] as const),
        ),
        skips: (sc.skip ?? []).filter((s) => onPlatform(s.platforms, os)).map((s) => s.surface),
      });
    }
  }
  return out;
}

/** Lifecycle checks the harness records in every run that got past session setup. */
export const ALWAYS_LIFECYCLE = ["install", "vibeke_side_stopped"];

/**
 * Checks a result against the trusted expectations. The result file is written by the same user
 * as the plugin, so the report requires the identity, the exact step list and the skipped
 * surfaces to match the scenario and the lock, and the fixed lifecycle checks to be present. It
 * cannot recompute which manifest surfaces the steps cover (the manifest is plugin data), so it
 * still relies on `coverage.ok`. Returns the problems; empty means consistent.
 */
export function crossCheck(r: PluginResult, e: Expected): string[] {
  const out: string[] = [];
  if (r.plugin_id !== e.plugin_id) out.push(`plugin_id ${r.plugin_id} is not ${e.plugin_id}`);
  if (r.repository !== e.repository) out.push(`repository ${r.repository} is not ${e.repository}`);
  if (r.rank !== e.rank) out.push(`rank ${r.rank} is not ${e.rank}`);
  if (r.os !== e.os) out.push(`os ${r.os} is not ${e.os}`);
  if (r.commit !== e.commit) out.push(`commit ${r.commit} is not the locked commit ${e.commit}`);
  // A run that already reports a harness error fails whatever else it says. It may stop early,
  // so it has no complete step list.
  const failedEarly = [...r.lifecycle, ...r.steps].some((x) => x.outcome === "harness_error");
  if (failedEarly) return out;
  const ids = r.steps.map((s) => s.id);
  const want = new Set(e.steps);
  const dup = ids.filter((id, i) => ids.indexOf(id) !== i);
  if (dup.length > 0) out.push(`duplicate steps: ${[...new Set(dup)].join(", ")}`);
  const missing = e.steps.filter((id) => !ids.includes(id));
  if (missing.length > 0) out.push(`missing scenario steps: ${missing.join(", ")}`);
  const extra = [...new Set(ids.filter((id) => !want.has(id)))];
  if (extra.length > 0) out.push(`steps that are not in the scenario: ${extra.join(", ")}`);
  // Known outcomes come from the trusted scenario; an unexpected pass is recomputed from them
  // (`reconcile` in vk-conformance), not taken from the file.
  for (const st of r.steps) {
    const k = e.known.get(st.id) ?? null;
    if (st.known !== k) out.push(`step ${st.id}: known ${st.known} is not the scenario's ${k}`);
    if (st.unexpected_pass !== (k !== null && st.outcome === "passed")) {
      out.push(`step ${st.id}: unexpected_pass disagrees with the scenario's known outcome`);
    }
  }
  const names = r.lifecycle.map((c) => c.check);
  for (const c of ALWAYS_LIFECYCLE) {
    const n = names.filter((x) => x === c).length;
    if (n !== 1) out.push(`lifecycle check ${c} appears ${n} times, not once`);
  }
  const skipped = [...r.coverage.skipped].sort().join("\u0000");
  if (skipped !== [...e.skips].sort().join("\u0000")) out.push("coverage.skipped is not the scenario's skip list for this platform");
  if (!r.coverage.ok || r.coverage.missing.length > 0) out.push("coverage is not complete");
  return out;
}

export const STATUS_SCHEMA = "vibeke.herdr-plugin-status/1";

/** What the trusted runner recorded about one plugin job, outside the boundary. */
export type Status = { plugin_id: string; os: string; exit_status: number | null; result_present: boolean };

export function validateStatus(v: unknown): Status {
  const o = object(v, "$", ["schema", "plugin_id", "os", "exit_status", "result_present"]);
  if (o.schema !== STATUS_SCHEMA) throw new Invalid("$.schema", "unknown schema");
  return {
    plugin_id: str(o.plugin_id, "$.plugin_id", LIMITS.id, PLUGIN_ID),
    os: oneOf(o.os, "$.os", ["linux", "macos"] as const),
    exit_status: o.exit_status === null ? null : uint(o.exit_status, "$.exit_status", 255),
    result_present: bool(o.result_present, "$.result_present"),
  };
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
  /** Plugins whose test exit status, recorded by the trusted runner, fails them. */
  exit_failures: { plugin_id: string; os: string; reason: string }[];
  missing: { plugin_id: string; os: string }[];
  /** Locked plugins that are not run for now, from the trusted lock. */
  deferred: Deferred[];
  failing: boolean;
};

export type Deferred = { rank: number; plugin_id: string; repository: string; reason: string };

const FAILED: Outcome[] = ["dependency_failure", "suspected_compat_failure", "compat_failure", "harness_error"];

function counts(outcomes: Outcome[]): Partial<Record<Outcome, number>> {
  const c: Partial<Record<Outcome, number>> = {};
  for (const o of outcomes) c[o] = (c[o] ?? 0) + 1;
  return c;
}

/** A matrix row. The outcome and `fails_run` are the recomputed ones, never the file's own. */
export function row(r: PluginResult, file: string, v: Verdict = verdict(r)): Row {
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
    outcome: v.outcome,
    reproducible: r.reproducible && r.dependency_mode === "verified",
    dependency_mode: r.dependency_mode,
    fails_run: v.fails_run,
    steps: counts(r.steps.map((s) => s.outcome)),
    lifecycle: counts(r.lifecycle.map((c) => c.outcome)),
    surfaces: [...r.surfaces].sort(),
    failed_surfaces: [...failed].sort(),
    partial_surfaces: [...partial].filter((s) => !failed.has(s)).sort(),
    file,
  };
}

export type Input = { file: string; text: string } | { file: string; error: string };

/**
 * Builds the matrix. `statuses` are the trusted status files; `null` skips that check (local
 * use without them). In CI every expected plugin needs a status with exit status 0 and a result.
 */
export function build(
  inputs: Input[],
  expect: { plugin_id: string; os: string }[] = [],
  statuses: Input[] | null = null,
  deferred: Deferred[] = [],
  trusted: Trusted | null = null,
): Matrix {
  const m: Matrix = {
    schema: MATRIX_SCHEMA,
    rows: [],
    unexpected_passes: [],
    coverage_failures: [],
    rejected: [],
    exit_failures: [],
    missing: [],
    deferred: [...deferred].sort((a, b) => a.rank - b.rank),
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
    if (trusted !== null) {
      const e = trusted.get(key);
      const bad = e ? crossCheck(r, e) : [`${r.plugin_id} on ${r.os} is not a locked, supported plugin and platform`];
      if (bad.length > 0) {
        m.rejected.push({
          file: input.file,
          reason: `harness_error: result does not match the trusted scenario and lock: ${bad.join("; ")}`,
        });
        continue;
      }
    }
    const v = verdict(r);
    const bad = inconsistencies(r, v);
    if (bad.length > 0) {
      m.rejected.push({ file: input.file, reason: `inconsistent result: ${bad.join("; ")}` });
      continue;
    }
    m.rows.push(row(r, input.file, v));
    for (const s of r.steps) {
      if (s.unexpected_pass) m.unexpected_passes.push({ plugin_id: r.plugin_id, os: r.os, step: s.id, known: s.known });
    }
    if (!r.coverage.ok) m.coverage_failures.push({ plugin_id: r.plugin_id, os: r.os, missing: r.coverage.missing });
  }
  for (const e of expect) {
    if (!seen.has(`${e.plugin_id}\u0000${e.os}`)) m.missing.push({ plugin_id: e.plugin_id, os: e.os });
  }
  if (statuses !== null) {
    const recorded = new Map<string, Status>();
    for (const input of statuses) {
      if ("error" in input) {
        m.rejected.push({ file: input.file, reason: input.error });
        continue;
      }
      let st: Status;
      try {
        st = validateStatus(JSON.parse(input.text));
      } catch (e) {
        const reason = e instanceof Invalid ? e.message : e instanceof SyntaxError ? "not valid JSON" : "unreadable";
        m.rejected.push({ file: input.file, reason });
        continue;
      }
      const key = `${st.plugin_id}\u0000${st.os}`;
      if (recorded.has(key)) {
        m.rejected.push({ file: input.file, reason: "duplicate status for this plugin and platform" });
        continue;
      }
      recorded.set(key, st);
    }
    const keys = new Map<string, { plugin_id: string; os: string }>();
    for (const e of expect) keys.set(`${e.plugin_id}\u0000${e.os}`, e);
    for (const r of m.rows) keys.set(`${r.plugin_id}\u0000${r.os}`, r);
    for (const st of recorded.values()) keys.set(`${st.plugin_id}\u0000${st.os}`, st);
    for (const [key, k] of keys) {
      const st = recorded.get(key);
      let reason: string | null = null;
      if (!st) reason = "no exit status was recorded (the plugin job did not finish)";
      else if (st.exit_status === null) reason = "the plugin test did not run to an exit status";
      else if (st.exit_status !== 0) reason = `the plugin test exited with status ${st.exit_status}`;
      else if (!st.result_present) reason = "the plugin test wrote no result file";
      if (reason) m.exit_failures.push({ plugin_id: k.plugin_id, os: k.os, reason });
    }
    m.exit_failures.sort((a, b) => a.plugin_id.localeCompare(b.plugin_id) || a.os.localeCompare(b.os));
  }
  m.rows.sort((a, b) => a.rank - b.rank || a.os.localeCompare(b.os) || a.plugin_id.localeCompare(b.plugin_id));
  m.failing =
    m.rejected.length > 0 ||
    m.missing.length > 0 ||
    m.exit_failures.length > 0 ||
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
    "Each result file is written inside the plugin's execution boundary, so the plugin can forge it. The report recomputes every verdict from the step details and also fails a plugin whose test exit status, recorded outside the boundary, is non-zero. A forged passing result can still hide a failure, so this matrix is a regression signal, not a security verdict (spec 18 §5.6).",
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
  if (m.exit_failures.length > 0) {
    lines.push("", "## Test exit status", "");
    for (const x of m.exit_failures) lines.push(`- ${md(x.plugin_id)} on ${md(x.os)}: ${md(x.reason)}.`);
  }
  if (m.deferred.length > 0) {
    lines.push("", "## Deferred", "");
    lines.push("These plugins keep their rank in the corpus, but the harness does not run them for now (spec 18 §6).", "");
    for (const d of m.deferred) {
      lines.push(`- ${d.rank}. ${md(d.plugin_id)} (${md(d.repository)}): ${md(d.reason)}`);
    }
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
export const STATUS_NAME = /^[A-Za-z0-9_.-]{1,200}-(linux|macos)\.status\.json$/;

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
 * Only regular files whose names match `names` (by default `<stem>-<os>.json`) count; symbolic links and other
 * file types are rejected without being followed.
 */
export function collect(dir: string, names: RegExp = RESULT_NAME): Input[] {
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
      if (!names.test(name)) continue;
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
    console.error(
      "usage: herdr-plugins-report.ts --in <dir> --out <dir> [--expect <matrix.json>] [--status <dir>] [--lock <lock.toml>] [--scenarios <dir>]",
    );
    process.exit(2);
  }
  const expectFile = arg(args, "--expect");
  // The expected matrix comes from the trusted plan job, not from the plugin jobs.
  const expect: { plugin_id: string; os: string }[] = expectFile
    ? (JSON.parse(await Bun.file(expectFile).text()).include ?? [])
    : [];
  // Status files from the trusted runner user (outside the boundary). Without --status the
  // exit-status check is skipped, which only local runs should do.
  const statusDir = arg(args, "--status");
  // The checked-in lock is trusted repository data, not a plugin job's output.
  const lockFile = arg(args, "--lock");
  const deferred = lockFile ? deferredPlugins(parseLock(await Bun.file(lockFile).text())) : [];
  // The scenarios come from the same trusted checkout as the lock.
  let trusted: Trusted | null = null;
  if (lockFile) {
    const dir = arg(args, "--scenarios") ?? join(dirname(lockFile), "scenarios");
    const scenarios = new Map<string, string>();
    for (const name of readdirSync(dir)) {
      if (name.endsWith(".toml")) scenarios.set(name.slice(0, -5), await Bun.file(join(dir, name)).text());
    }
    trusted = trustedFrom(parseLock(await Bun.file(lockFile).text()), scenarios);
  }
  const m = build(collect(input), expect, statusDir ? collect(statusDir, STATUS_NAME) : null, deferred, trusted);
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, "matrix.json"), `${JSON.stringify(m, null, 2)}\n`);
  writeFileSync(join(out, "matrix.md"), render(m));
  console.log(
    `${m.rows.length} results, ${m.rejected.length} rejected, ${m.missing.length} missing, ${m.exit_failures.length} failed test exit statuses: ${m.failing ? "failing" : "passing"}`,
  );
  process.exit(m.failing ? 1 : 0);
}
