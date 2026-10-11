#!/usr/bin/env bun
// Merges a record run's dependency lines into the plugin lock (spec 18 §7.2).
//
//   bun scripts/herdr-plugins-record.ts --plugin <plugin_id> [--lock L] <record.jsonl>...
//
// A record run of the herdr-plugins workflow (`record: true`) sends every download through
// `scripts/herdr-plugins/verified_proxy.py` and uploads its `{url, sha256}` lines per plugin and
// platform. This script replaces the plugin's `[[plugin.dependency]]` entries with the union of
// those lines and sets `dependency_mode = "verified"`. A person reviews the diff before it is
// committed. Plugins that use protocols other than HTTP or pin certificates stay `unpinned`
// (18 §15); do not merge their record runs.

import { readFileSync, writeFileSync } from "node:fs";
import { DEFAULT_LOCK, parseLock } from "./herdr-plugins/lock-tools";

export type Dependency = { url: string; sha256: string };

/** Parses record lines, keeping valid `{url, sha256}` pairs, deduplicated and sorted. */
export function parseRecords(texts: string[]): Dependency[] {
  const seen = new Map<string, Dependency>();
  for (const text of texts) {
    for (const line of text.split("\n")) {
      if (!line.trim()) continue;
      const v = JSON.parse(line) as { url?: unknown; sha256?: unknown };
      if (typeof v.url !== "string" || !/^https?:\/\/[^\s"\\]+$/.test(v.url) || v.url.length > 2048) {
        throw new Error(`bad url in record line: ${line.slice(0, 120)}`);
      }
      if (typeof v.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(v.sha256)) {
        throw new Error(`bad sha256 in record line: ${line.slice(0, 120)}`);
      }
      seen.set(`${v.url} ${v.sha256}`, { url: v.url, sha256: v.sha256 });
    }
  }
  return [...seen.values()].sort((a, b) => (a.url < b.url ? -1 : a.url > b.url ? 1 : a.sha256 < b.sha256 ? -1 : 1));
}

const HEADER = /^\s*\[/;
const PLUGIN = /^\s*\[\[\s*plugin\s*\]\]\s*(#.*)?$/;
const DEPENDENCY = /^\s*\[\[\s*plugin\.dependency\s*\]\]\s*(#.*)?$/;

/** Rewrites the lock text: the plugin's dependencies become `deps`, its mode `verified`. */
export function merge(lockText: string, pluginId: string, deps: Dependency[]): string {
  const lines = lockText.split("\n");
  const starts = lines.flatMap((l, i) => (PLUGIN.test(l) ? [i] : []));
  let start = -1;
  let end = -1;
  for (const s of starts) {
    let e = s + 1;
    while (e < lines.length && !(HEADER.test(lines[e]) && !DEPENDENCY.test(lines[e]))) e++;
    const body = lines.slice(s, e);
    if (body.some((l) => new RegExp(`^\\s*plugin_id\\s*=\\s*${JSON.stringify(pluginId).replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*$`).test(l))) {
      start = s;
      end = e;
      break;
    }
  }
  if (start < 0) throw new Error(`plugin ${pluginId} is not in the lock`);

  const kept: string[] = [];
  let inDependency = false;
  for (const l of lines.slice(start, end)) {
    if (DEPENDENCY.test(l)) inDependency = true;
    else if (HEADER.test(l)) inDependency = false;
    if (inDependency) continue;
    kept.push(/^\s*dependency_mode\s*=/.test(l) ? 'dependency_mode = "verified"' : l);
  }
  while (kept.length > 0 && kept[kept.length - 1].trim() === "") kept.pop();
  const block = [...kept];
  for (const d of deps) {
    block.push("", "[[plugin.dependency]]", `url = ${JSON.stringify(d.url)}`, `sha256 = ${JSON.stringify(d.sha256)}`);
  }
  const after = lines.slice(end);
  if (after.length > 0 && !(after.length === 1 && after[0] === "")) block.push("");
  return [...lines.slice(0, start), ...block, ...after].join("\n");
}

if (import.meta.main) {
  const args = Bun.argv.slice(2);
  let lockPath = DEFAULT_LOCK;
  let pluginId = "";
  const files: string[] = [];
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--lock") lockPath = args[++i];
    else if (args[i] === "--plugin") pluginId = args[++i];
    else files.push(args[i]);
  }
  if (!pluginId || files.length === 0) {
    console.error("usage: herdr-plugins-record.ts --plugin <plugin_id> [--lock L] <record.jsonl>...");
    process.exit(2);
  }
  const deps = parseRecords(files.map((f) => readFileSync(f, "utf8")));
  const next = merge(readFileSync(lockPath, "utf8"), pluginId, deps);
  const lock = parseLock(next);
  const p = (lock.plugin ?? []).find((q) => q.plugin_id === pluginId);
  if (!p || p.dependency_mode !== "verified" || (p.dependency ?? []).length !== deps.length) {
    throw new Error("the merged lock does not parse back as expected");
  }
  writeFileSync(lockPath, next);
  console.log(`${pluginId}: ${deps.length} dependencies, mode verified. Review the diff before committing.`);
}
