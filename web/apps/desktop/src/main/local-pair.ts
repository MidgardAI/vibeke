// "Connect to this Mac" (spec 16 §16.1): run `vibeke gateway pair --local`, which prints
// `{link, d, pid, socket}` with `link.relay = "local:<socket>"`, and pair over the gateway's Unix
// socket. Same user, so no fingerprint confirmation is needed (the CLI marks it no-confirm).

import { execFile, spawn } from 'node:child_process';
import { mkdirSync, openSync, realpathSync, statSync } from 'node:fs';
import { connect } from 'node:net';
import { homedir } from 'node:os';
import { delimiter, dirname, join, posix, win32 } from 'node:path';
import { localSocketPath, parseLink, type PairingLink } from '@vibeke/core';

export interface LocalPairOutput {
  link: PairingLink;
  socket: string;
  pid: string;
}

/** Validate the CLI's JSON (last JSON line of stdout; logs go to stderr). */
export function parsePairOutput(stdout: string): LocalPairOutput {
  const line = stdout
    .split('\n')
    .map((l) => l.trim())
    .reverse()
    .find((l) => l.startsWith('{'));
  if (!line) throw new Error('no JSON in `vibeke gateway pair --local` output');
  let o: unknown;
  try {
    o = JSON.parse(line);
  } catch {
    throw new Error('`vibeke gateway pair --local` printed invalid JSON');
  }
  const r = o as { d?: unknown; socket?: unknown; pid?: unknown };
  if (typeof r.d !== 'string' || typeof r.socket !== 'string' || typeof r.pid !== 'string') throw new Error('unexpected `pair --local` output');
  const link = parseLink(r.d);
  const sock = localSocketPath(link.relay);
  if (!sock) throw new Error('`pair --local` returned a non-local link');
  if (sock !== r.socket) throw new Error('`pair --local` link and socket disagree');
  if (link.pid !== r.pid) throw new Error('`pair --local` pairing id mismatch');
  return { link, socket: sock, pid: r.pid };
}

/** What the trust check needs from the file system (injected in tests). */
export interface FsProbe {
  realpath(p: string): string;
  stat(p: string): { isFile(): boolean; isDirectory(): boolean; mode: number; uid: number; gid?: number };
  /** The current user's uid; null on Windows (no POSIX ownership or mode bits). */
  uid: number | null;
}

export const nodeFs: FsProbe = {
  realpath: (p) => realpathSync(p),
  stat: (p) => statSync(p),
  uid: typeof process.getuid === 'function' ? process.getuid() : null,
};

export type ExecCheck = { ok: true; path: string } | { ok: false; reason: string; missing?: boolean };

const pathFor = (platform: string) => (platform === 'win32' ? win32 : posix);

/**
 * May this file be executed as the `vibeke` CLI? Absolute, canonical (symlinks resolved), a
 * regular executable file owned by this user or root and not writable by group/others, and every
 * directory from its parent up to `/` owned by this user or root and not writable by group or
 * others. Two exceptions: a root-owned sticky directory (e.g. `/tmp`) higher up the chain (never
 * the immediate parent), and on macOS a Homebrew prefix (`/opt/homebrew`, `/usr/local`) writable
 * by the `admin` group (gid 80), as Homebrew sets it up: admins can already run anything via sudo,
 * so this adds no attacker. Provenance beyond that (is it really Vibeke?) is `vibekeVersion`'s job.
 *
 * Windows has no POSIX ownership or mode bits: only the structural checks apply here, and
 * `discoverVibeke` trusts nothing outside the standard install roots without the native picker.
 */
export function checkExecutable(p: string, fs: FsProbe = nodeFs, platform: string = process.platform): ExecCheck {
  const P = pathFor(platform);
  if (!p || !P.isAbsolute(p)) return { ok: false, reason: 'not an absolute path' };
  let real: string;
  try {
    real = fs.realpath(p);
  } catch {
    return { ok: false, reason: 'does not exist', missing: true };
  }
  let st: ReturnType<FsProbe['stat']>;
  try {
    st = fs.stat(real);
  } catch {
    return { ok: false, reason: 'cannot be read', missing: true };
  }
  if (!st.isFile()) return { ok: false, reason: 'is not a regular file' };
  if (fs.uid === null) return { ok: true, path: real };
  if ((st.mode & 0o111) === 0) return { ok: false, reason: 'is not executable' };
  if (st.uid !== fs.uid && st.uid !== 0) return { ok: false, reason: 'is owned by another user' };
  if ((st.mode & 0o022) !== 0) return { ok: false, reason: 'is writable by other users' };
  const bad = checkAncestors(P.dirname(real), fs, P, platform);
  return bad ? { ok: false, reason: bad } : { ok: true, path: real };
}

const HOMEBREW_PREFIXES = ['/opt/homebrew', '/usr/local'];

/** Walk from `dir` up to the root; the first untrusted directory's reason, or null. */
function checkAncestors(dir: string, fs: FsProbe, P: typeof posix, platform: string = process.platform): string | null {
  let d = dir;
  for (let immediate = true; ; immediate = false) {
    let st: ReturnType<FsProbe['stat']>;
    try {
      st = fs.stat(d);
    } catch {
      return `is under ${d}, which cannot be read`;
    }
    const where = immediate ? 'is in a directory' : `is under ${d}, a directory`;
    if (!st.isDirectory()) return `is under ${d}, which is not a directory`;
    if (st.uid !== fs.uid && st.uid !== 0) return `${where} owned by another user`;
    if ((st.mode & 0o022) !== 0) {
      const stickyRoot = (st.mode & 0o1000) !== 0 && st.uid === 0;
      const homebrewAdmin =
        platform === 'darwin' &&
        (st.mode & 0o002) === 0 &&
        st.gid === 80 &&
        HOMEBREW_PREFIXES.some((pre) => d === pre || d.startsWith(`${pre}/`));
      if (homebrewAdmin) {
        // Homebrew's own layout: group admin may write; nobody else.
      } else if (immediate || !stickyRoot) return (st.mode & 0o002) !== 0 ? `${where} anyone can write to` : `${where} other users can write to`;
    }
    const parent = P.dirname(d);
    if (parent === d) return null;
    d = parent;
  }
}

/** The usual install locations, in preference order (GUI apps on macOS get a minimal PATH). */
export function knownDirs(home = homedir()): string[] {
  return [join(home, '.local/bin'), '/opt/homebrew/bin', '/usr/local/bin', join(home, '.cargo/bin'), '/usr/bin'];
}

/**
 * Windows: the only locations trusted without the native picker are the standard install roots
 * (`%ProgramFiles%`, `%ProgramFiles(x86)%` and `%LOCALAPPDATA%\Programs`; only administrators,
 * or the user's own installers, write there). Anything else — `$VIBEKE_BIN`, `~\.cargo\bin`,
 * other PATH entries — must be confirmed by choosing it in the picker.
 */
export function windowsInstallRoots(env: NodeJS.ProcessEnv): string[] {
  const roots = [env.ProgramFiles, env['ProgramFiles(x86)'], env.LOCALAPPDATA ? win32.join(env.LOCALAPPDATA, 'Programs') : undefined];
  return roots.filter((r): r is string => !!r && win32.isAbsolute(r)).map((r) => win32.resolve(r).toLowerCase());
}

const underWindowsRoot = (p: string, roots: string[]) => {
  const lp = win32.resolve(p).toLowerCase();
  return roots.some((r) => lp.startsWith(r.endsWith('\\') ? r : `${r}\\`));
};

export type Discovery =
  /** Ready to run (chosen in the picker, `$VIBEKE_BIN`, or a usual install location). */
  | { kind: 'found'; path: string }
  /** Only found outside the usual locations: the user confirms it in the picker. */
  | { kind: 'unexpected'; path: string }
  /** The chosen / configured / installed executable fails the checks. */
  | { kind: 'invalid'; path: string; reason: string }
  | { kind: 'missing' };

const exe = (platform: string) => (platform === 'win32' ? 'vibeke.exe' : 'vibeke');

/**
 * Find the CLI: the executable chosen in the picker (an invalid one is reported, never skipped),
 * `$VIBEKE_BIN`, the usual install locations, and last PATH. Relative PATH entries are ignored
 * (they would resolve against whatever directory the app was started in). On Windows, anything
 * outside the standard install roots is `unexpected` (needs the picker); see `windowsInstallRoots`.
 */
export function discoverVibeke(env: NodeJS.ProcessEnv, explicit: string, o: { home?: string; fs?: FsProbe; platform?: string } = {}): Discovery {
  const fs = o.fs ?? nodeFs;
  const platform = o.platform ?? process.platform;
  const P = pathFor(platform);
  const name = exe(platform);
  const check = (p: string) => checkExecutable(p, fs, platform);
  if (explicit) {
    const c = check(explicit);
    return c.ok ? { kind: 'found', path: c.path } : { kind: 'invalid', path: explicit, reason: c.reason };
  }
  if (platform === 'win32') return discoverWindows(env, fs);
  let firstBad: Discovery | null = null;
  const tryTrusted = (p: string): Discovery | null => {
    const c = check(p);
    if (c.ok) return { kind: 'found', path: c.path };
    if (!c.missing) firstBad ??= { kind: 'invalid', path: p, reason: c.reason };
    return null;
  };
  const fromEnv = env.VIBEKE_BIN;
  if (fromEnv && P.isAbsolute(fromEnv)) {
    const r = tryTrusted(fromEnv);
    if (r) return r;
  }
  const known = knownDirs(o.home);
  for (const d of known) {
    const r = tryTrusted(P.join(d, name));
    if (r) return r;
  }
  const knownReal = new Set(known.map((d) => {
    try {
      return fs.realpath(d);
    } catch {
      return d;
    }
  }));
  for (const d of (env.PATH ?? '').split(delimiter)) {
    if (!d || !P.isAbsolute(d)) continue;
    const c = check(P.join(d, name));
    if (!c.ok) {
      if (!c.missing) firstBad ??= { kind: 'invalid', path: P.join(d, name), reason: c.reason };
      continue;
    }
    if (knownReal.has(P.dirname(c.path))) return { kind: 'found', path: c.path };
    return { kind: 'unexpected', path: c.path };
  }
  return firstBad ?? { kind: 'missing' };
}

/** Windows discovery: `$VIBEKE_BIN` then PATH (`;`-separated); only install roots are trusted. */
function discoverWindows(env: NodeJS.ProcessEnv, fs: FsProbe): Discovery {
  const roots = windowsInstallRoots(env);
  const candidates: string[] = [];
  if (env.VIBEKE_BIN && win32.isAbsolute(env.VIBEKE_BIN)) candidates.push(env.VIBEKE_BIN);
  for (const d of (env.PATH ?? env.Path ?? '').split(';')) if (d && win32.isAbsolute(d)) candidates.push(win32.join(d, 'vibeke.exe'));
  let unexpected: Discovery | null = null;
  let firstBad: Discovery | null = null;
  for (const p of candidates) {
    const c = checkExecutable(p, fs, 'win32');
    if (!c.ok) {
      if (!c.missing) firstBad ??= { kind: 'invalid', path: p, reason: c.reason };
      continue;
    }
    if (underWindowsRoot(c.path, roots)) return { kind: 'found', path: c.path };
    unexpected ??= { kind: 'unexpected', path: c.path };
  }
  return unexpected ?? firstBad ?? { kind: 'missing' };
}

/** `<bin> --version` must print `vibeke <semver>`; returns that version, or throws. */
export function vibekeVersion(bin: string, env: NodeJS.ProcessEnv): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(bin, ['--version'], { env: cliEnv(bin, env), timeout: 5_000, maxBuffer: 64 * 1024 }, (err, stdout) => {
      const m = /^vibeke (\d+\.\d+\.\d+\S*)\s*$/.exec(String(stdout).trim());
      if (err || !m) return reject(new Error('This is not the vibeke command (`--version` did not print a Vibeke version).'));
      resolve(m[1]!);
    });
  });
}

/** The environment for the CLI: ours plus the bin dir on PATH (for the CLI's own helpers). */
function cliEnv(bin: string, env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return { ...env, PATH: [dirname(bin), env.PATH ?? ''].filter(Boolean).join(delimiter) };
}

export function runPairLocal(bin: string, env: NodeJS.ProcessEnv): Promise<LocalPairOutput> {
  return new Promise((resolve, reject) => {
    execFile(bin, ['gateway', 'pair', '--local'], { env: cliEnv(bin, env), timeout: 20_000, maxBuffer: 1 << 20 }, (err, stdout, stderr) => {
      if (err) {
        const msg = String(stderr || err.message).trim().split('\n').slice(-3).join('\n');
        return reject(Object.assign(new Error(msg || 'vibeke gateway pair --local failed'), { code: 'cli_failed' }));
      }
      try {
        resolve(parsePairOutput(stdout));
      } catch (e) {
        reject(Object.assign(e as Error, { code: 'bad_output' }));
      }
    });
  });
}

/** True when something accepts connections on the Unix socket. */
export function socketAlive(path: string, timeoutMs = 1500): Promise<boolean> {
  return new Promise((resolve) => {
    const s = connect({ path });
    const done = (v: boolean) => {
      s.destroy();
      resolve(v);
    };
    s.setTimeout(timeoutMs, () => done(false));
    s.once('connect', () => done(true));
    s.once('error', () => done(false));
  });
}

export async function waitForSocket(path: string, ms: number): Promise<boolean> {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    if (await socketAlive(path, 500)) return true;
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

/** Start `vibeke gateway run` detached (it outlives the app; logs to `logFile`). */
export function startGateway(bin: string, env: NodeJS.ProcessEnv, logFile: string): void {
  mkdirSync(dirname(logFile), { recursive: true });
  const fd = openSync(logFile, 'a', 0o600);
  const child = spawn(bin, ['gateway', 'run'], { env: cliEnv(bin, env), detached: true, stdio: ['ignore', fd, fd] });
  child.unref();
}

/** The gateway state dir, as `vk-gateway` computes it (crates/vk-gateway/src/state.rs). */
export function defaultGatewayDir(env: NodeJS.ProcessEnv, platform: string = process.platform, home = homedir()): string {
  if (env.VIBEKE_GATEWAY_DIR) return env.VIBEKE_GATEWAY_DIR;
  const base = platform === 'darwin' ? join(home, 'Library/Application Support') : env.XDG_CONFIG_HOME || join(home, '.config');
  return join(base, 'vibeke/gateway');
}

export const gatewaySocket = (env: NodeJS.ProcessEnv): string => join(defaultGatewayDir(env), 'gateway.sock');
