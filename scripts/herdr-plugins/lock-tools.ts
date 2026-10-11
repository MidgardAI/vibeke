#!/usr/bin/env bun
// Reads the locked plugin corpus (spec 18 §6) for the herdr-plugins workflow. Trusted: it only
// reads the checked-in lock, never plugin code or result artifacts.
//
//   bun scripts/herdr-plugins/lock-tools.ts matrix [--lock L] [--filter "ids, repos or ranks"] [--os linux,macos]
//   bun scripts/herdr-plugins/lock-tools.ts toolchains [--lock L]
//   bun scripts/herdr-plugins/lock-tools.ts allowlist [--lock L] --plugin <plugin_id>
//
// `matrix` prints `{"include": [...]}` with one entry per plugin and OS; deferred plugins
// (`[[defer]]` in the lock, 18 §6) are left out. `toolchains` prints `name=version` lines.
// `allowlist` prints the verified-mode allowlist the proxy reads (18 §7.2).

import { readFileSync } from "node:fs";

export const DEFAULT_LOCK = "tests/compat/herdr/0.9.3/plugins/top20.lock.toml";

export type LockedPlugin = {
  rank: number;
  repository: string;
  repo_id: number;
  plugin_id: string;
  manifest_path: string;
  commit: string;
  manifest_sha256: string;
  platforms?: string[];
  dependency_mode: "verified" | "unpinned";
  dependency?: { url: string; sha256: string }[];
};

export type Deferral = { repository: string; reason: string };

export type Lock = {
  schema: number;
  toolchains?: Record<string, string>;
  defer?: Deferral[];
  plugin?: LockedPlugin[];
};

/** Why `p` is deferred (its repository is in the lock's `defer` list), or `undefined`. */
export function deferral(lock: Lock, p: LockedPlugin): Deferral | undefined {
  return (lock.defer ?? []).find((d) => d.repository === p.repository);
}

/** The deferred plugins in rank order, with their reasons, for the report. */
export function deferredPlugins(lock: Lock): { rank: number; plugin_id: string; repository: string; reason: string }[] {
  return [...(lock.plugin ?? [])]
    .sort((a, b) => a.rank - b.rank)
    .flatMap((p) => {
      const d = deferral(lock, p);
      return d ? [{ rank: p.rank, plugin_id: p.plugin_id, repository: p.repository, reason: d.reason }] : [];
    });
}

export type MatrixEntry = {
  plugin_id: string;
  repository: string;
  rank: number;
  stem: string;
  os: "linux" | "macos";
  runner: string;
  mode: "verified" | "unpinned";
  /** nextest filter for the plugin test: `plugin_<rank:02>_…` (18 §12). */
  filter: string;
};

export const RUNNERS: Record<"linux" | "macos", string> = {
  linux: "ubuntu-latest",
  macos: "macos-latest",
};

export function parseLock(text: string): Lock {
  const lock = Bun.TOML.parse(text) as Lock;
  if (lock.schema !== 1) throw new Error(`unsupported lock schema ${lock.schema}`);
  return lock;
}

/** `owner__repo`, plus `__<plugin_id>` when the repository has several locked manifests. */
export function stem(lock: Lock, p: LockedPlugin): string {
  const [owner, repo] = p.repository.split("/");
  const shared = (lock.plugin ?? []).filter((q) => q.repository === p.repository).length > 1;
  // Same rule as `LockedPlugin::stem` in crates/vk-conformance/src/lock.rs.
  const id = p.plugin_id.replace(/[^A-Za-z0-9_.-]/g, "_");
  return shared ? `${owner}__${repo}__${id}` : `${owner}__${repo}`;
}

export function supports(p: LockedPlugin, os: string): boolean {
  return !p.platforms || p.platforms.length === 0 || p.platforms.includes(os);
}

/** Splits the dispatch input on commas and whitespace; empty selects every plugin. */
export function selects(filter: string, p: LockedPlugin): boolean {
  const terms = filter.split(/[\s,]+/).filter(Boolean);
  if (terms.length === 0) return true;
  return terms.some(
    (t) => t === p.plugin_id || t === p.repository || (/^\d+$/.test(t) && Number(t) === p.rank),
  );
}

export function matrix(lock: Lock, filter: string, oses: ("linux" | "macos")[]): MatrixEntry[] {
  const out: MatrixEntry[] = [];
  const plugins = [...(lock.plugin ?? [])].sort((a, b) => a.rank - b.rank);
  for (const p of plugins) {
    if (!selects(filter, p) || deferral(lock, p)) continue;
    if (!/^[A-Za-z0-9][A-Za-z0-9-]*\/(?!\.\.?$)[A-Za-z0-9_.-]+$/.test(p.repository)) {
      throw new Error(`bad repository ${JSON.stringify(p.repository)}`);
    }
    if (!/^[A-Za-z0-9_.-]+$/.test(p.plugin_id)) {
      throw new Error(`bad plugin id ${JSON.stringify(p.plugin_id)}`);
    }
    for (const os of oses) {
      if (!supports(p, os)) continue;
      out.push({
        plugin_id: p.plugin_id,
        repository: p.repository,
        rank: p.rank,
        stem: stem(lock, p),
        os,
        runner: RUNNERS[os],
        mode: p.dependency_mode,
        filter: `test(/^compat_herdr_real_plugins::plugin_${String(p.rank).padStart(2, "0")}_/)`,
      });
    }
  }
  return out;
}

export function allowlist(lock: Lock, pluginId: string) {
  const p = (lock.plugin ?? []).find((q) => q.plugin_id === pluginId);
  if (!p) throw new Error(`plugin ${pluginId} is not in the lock`);
  return { plugin_id: p.plugin_id, mode: p.dependency_mode, dependencies: p.dependency ?? [] };
}

function arg(args: string[], name: string, fallback?: string): string {
  const i = args.indexOf(name);
  if (i >= 0 && i + 1 < args.length) return args[i + 1];
  if (fallback !== undefined) return fallback;
  throw new Error(`missing ${name}`);
}

if (import.meta.main) {
  const [cmd, ...args] = Bun.argv.slice(2);
  const lock = parseLock(readFileSync(arg(args, "--lock", DEFAULT_LOCK), "utf8"));
  switch (cmd) {
    case "matrix": {
      const oses = arg(args, "--os", "linux,macos")
        .split(",")
        .filter((o): o is "linux" | "macos" => o === "linux" || o === "macos");
      const include = matrix(lock, arg(args, "--filter", ""), oses);
      console.log(JSON.stringify({ include }));
      break;
    }
    case "toolchains":
      for (const [name, version] of Object.entries(lock.toolchains ?? {})) {
        if (!/^[a-z]+$/.test(name) || !/^[0-9A-Za-z.+-]+$/.test(version)) {
          throw new Error(`bad toolchain pin ${name}=${version}`);
        }
        console.log(`${name}=${version}`);
      }
      break;
    case "allowlist":
      console.log(JSON.stringify(allowlist(lock, arg(args, "--plugin"))));
      break;
    default:
      console.error("usage: lock-tools.ts matrix|toolchains|allowlist [--lock L] …");
      process.exit(2);
  }
}
