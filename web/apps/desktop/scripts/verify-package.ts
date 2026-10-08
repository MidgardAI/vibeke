// Verify a packaged build (spec 16 §16.2, review item 13): run after `bun run dist:desktop -- --dir`
// (or any `dist*`). For every unpacked app under dist/ (or the paths given as arguments):
//   - reads the Electron fuse wire with @electron/fuses and asserts the hardening fuses;
//   - checks the app ships as app.asar only (no loose app/ directory);
//   - macOS: `codesign --verify --deep --strict` on the .app, and the asar integrity hash is
//     embedded in Info.plist (required by EnableEmbeddedAsarIntegrityValidation).
// Exits non-zero on any failure.

import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { extractFile } from '@electron/asar';
import { FuseV1Options, getCurrentFuseWire } from '@electron/fuses';

const root = resolve(import.meta.dir, '..');
const dist = join(root, 'dist');

/** The fuse values the shipped app must have (mirrors electronFuses in electron-builder.config.cjs). */
export const EXPECTED_FUSES: Partial<Record<FuseV1Options, boolean>> = {
  [FuseV1Options.RunAsNode]: false,
  [FuseV1Options.EnableCookieEncryption]: true,
  [FuseV1Options.EnableNodeOptionsEnvironmentVariable]: false,
  [FuseV1Options.EnableNodeCliInspectArguments]: false,
  [FuseV1Options.EnableEmbeddedAsarIntegrityValidation]: true,
  [FuseV1Options.OnlyLoadAppFromAsar]: true,
  [FuseV1Options.GrantFileProtocolExtraPrivileges]: false,
};

/** @electron/fuses' FuseState (not re-exported by the package): ASCII '0' / '1' / 'r', and 0x90. */
const FuseState = { DISABLE: 48, ENABLE: 49, REMOVED: 114, INHERIT: 144 } as const;

const SENTINEL = Buffer.from('dL7pKGdnNz796PbbjQWNKmHXBZaB9tsX');

interface Target {
  /** What @electron/fuses reads: the .app on macOS, the main executable elsewhere. */
  fusePath: string;
  /** The resources directory (app.asar). */
  resources: string;
  /** macOS bundle to codesign-verify. */
  app?: string;
}

function findTargets(args: string[]): Target[] {
  const out: Target[] = [];
  const consider = (p: string) => {
    if (p.endsWith('.app')) return void out.push({ fusePath: p, resources: join(p, 'Contents', 'Resources'), app: p });
    if (!statSync(p).isDirectory()) return;
    const entries = readdirSync(p);
    const apps = entries.filter((e) => e.endsWith('.app'));
    if (apps.length) return apps.forEach((e) => consider(join(p, e)));
    if (entries.includes('resources') && existsSync(join(p, 'resources', 'app.asar'))) {
      // Linux / Windows unpacked dir: the main executable is the one carrying the fuse sentinel.
      const exe = entries
        .map((e) => join(p, e))
        .filter((f) => statSync(f).isFile() && statSync(f).size > 10_000_000)
        .find((f) => readFileSync(f).includes(SENTINEL));
      if (exe) out.push({ fusePath: exe, resources: join(p, 'resources') });
      return;
    }
    for (const e of entries) if (/^(mac|linux|win)/.test(e)) consider(join(p, e));
  };
  if (args.length) args.forEach((a) => consider(resolve(a)));
  else if (existsSync(dist)) consider(dist);
  return out;
}

const fuseName = (i: number) => FuseV1Options[i] ?? `fuse ${i}`;
const stateName = (s: number) => (s === FuseState.ENABLE ? 'enabled' : s === FuseState.DISABLE ? 'disabled' : s === FuseState.REMOVED ? 'removed' : s === FuseState.INHERIT ? 'inherit' : `0x${s.toString(16)}`);

async function verify(t: Target): Promise<string[]> {
  const errors: string[] = [];
  const wire = (await getCurrentFuseWire(t.fusePath)) as unknown as Record<number, number> & { version: string };
  if (wire.version !== '1') errors.push(`unexpected fuse wire version ${wire.version}`);
  for (const [k, want] of Object.entries(EXPECTED_FUSES)) {
    const i = Number(k);
    const got = wire[i];
    const ok = got === (want ? FuseState.ENABLE : FuseState.DISABLE);
    console.log(`  ${ok ? 'ok ' : 'BAD'} ${fuseName(i)}: ${got === undefined ? 'missing' : stateName(got)} (want ${want ? 'enabled' : 'disabled'})`);
    if (!ok) errors.push(`${fuseName(i)} is ${got === undefined ? 'missing' : stateName(got)}, want ${want ? 'enabled' : 'disabled'}`);
  }
  if (!existsSync(join(t.resources, 'app.asar'))) errors.push('resources/app.asar is missing');
  if (existsSync(join(t.resources, 'app'))) errors.push('a loose resources/app directory ships next to app.asar');
  if (t.app) {
    const plist = readFileSync(join(t.app, 'Contents', 'Info.plist'), 'utf8');
    if (!plist.includes('ElectronAsarIntegrity')) errors.push('Info.plist has no ElectronAsarIntegrity (asar integrity validation would fail)');
    if (process.platform === 'darwin') {
      try {
        execFileSync('codesign', ['--verify', '--deep', '--strict', '--verbose=2', t.app], { stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8' });
        console.log('  ok  codesign --verify --deep --strict');
        const policy = JSON.parse(extractFile(join(t.resources, 'app.asar'), 'out/main/update-policy.json').toString());
        if (policy.macSigned) {
          // Require the stapled notarization ticket and Gatekeeper assessment, not just
          // the build-time environment flag.
          execFileSync('xcrun', ['stapler', 'validate', t.app], { stdio: ['ignore', 'pipe', 'pipe'] });
          execFileSync('spctl', ['--assess', '--type', 'execute', '--verbose=2', t.app], { stdio: ['ignore', 'pipe', 'pipe'] });
          console.log('  ok  notarization ticket and Gatekeeper assessment');
        }
      } catch (e) {
        const err = e as { stderr?: string; message: string };
        errors.push(`macOS package signature/notarization verification failed: ${(err.stderr || err.message).trim()}`);
      }
    }
  }
  return errors;
}

const targets = findTargets(process.argv.slice(2));
if (!targets.length) {
  console.error('verify:package: no packaged app found. Run `bun run dist:desktop -- --dir` first (or pass the .app / unpacked dir).');
  process.exit(1);
}
let failed = false;
for (const t of targets) {
  console.log(`${t.app ?? t.fusePath}`);
  const errors = await verify(t);
  for (const e of errors) console.error(`  FAIL ${e}`);
  failed ||= errors.length > 0;
}
if (failed) process.exit(1);
console.log(`verify:package: ${targets.length} package(s) ok`);
