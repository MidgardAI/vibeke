import { describe, expect, test } from 'bun:test';
import { createHash, generateKeyPairSync, sign } from 'node:crypto';
import { UpdateController } from '../src/main/update-controller';
import { UnsupportedUpdatePlatform, RELEASE_API, RELEASE_REPO, discoverRelease, newer, verifyMinisign, verifiedSums, type DesktopRelease } from '../src/main/update-release';
import { checkedInfo } from '../src/main/update-info';
import { updateLabel } from '../../../packages/ui/src/components/updates';

// Ephemeral test keys; the production bundle's trust roots always come from bootstrap.rs.
function signer() {
  const { privateKey, publicKey } = generateKeyPairSync('ed25519');
  const id = Buffer.from('testonly');
  const key = Buffer.concat([Buffer.from('Ed'), id, (publicKey.export({ format: 'der', type: 'spki' }) as Buffer).subarray(-32)]).toString('base64');
  return { key, signature(data: Buffer, comment = 'vibeke v0.3.0') {
    const sig = sign(null, createHash('blake2b512').update(data).digest(), privateKey);
    const global = sign(null, Buffer.concat([sig, Buffer.from(comment)]), privateKey);
    return `untrusted comment: test\n${Buffer.concat([Buffer.from('ED'), id, sig]).toString('base64')}\ntrusted comment: ${comment}\n${global.toString('base64')}\n`;
  } };
}
function fixture() {
  const s = signer(), version = '0.3.0', base = `${RELEASE_REPO}/releases/download/v${version}`;
  const name = `Vibeke-${version}-mac-arm64.zip`;
  const payload = Buffer.from('test payload');
  const channel = `version: ${version}\nfiles:\n  - url: ${name}\n    sha512: ${createHash('sha512').update(payload).digest('base64')}\n    size: ${payload.length}\n`;
  const files = new Map<string, Buffer>([['latest-mac.yml', Buffer.from(channel)], [name, payload], [`Vibeke-${version}-mac-arm64.dmg`, payload]]);
  const sums = Buffer.from([...files].map(([n, b]) => `${createHash('sha256').update(b).digest('hex')}  ${n}`).join('\n') + '\n');
  files.set('SHA256SUMS', sums); files.set('SHA256SUMS.minisig', Buffer.from(s.signature(sums)));
  const meta = { tag_name: `v${version}`, draft: false, prerelease: false, assets: [...files.keys()].map((name) => ({ name, browser_download_url: `${base}/${name}` })) };
  const get = async (url: string) => {
    if (url === RELEASE_API) return Buffer.from(JSON.stringify(meta));
    if (!url.startsWith(`${base}/`) || !files.has(url.slice(base.length + 1))) throw new Error('unexpected URL');
    return files.get(url.slice(base.length + 1))!;
  };
  return { s, files, meta, get };
}
describe('authenticated desktop releases', () => {
  test('checks the signature, trusted version and exact metadata bytes before parsing', async () => {
    const f = fixture();
    const release = await discoverRelease('darwin', 'arm64', [f.s.key], f.get);
    expect(checkedInfo(release).version).toBe('0.3.0');
    f.files.set('latest-mac.yml', Buffer.from('tampered'));
    await expect(discoverRelease('darwin', 'arm64', [f.s.key], f.get)).rejects.toThrow('checksum');
  });
  test('rejects wrong key, tampered trusted comment, version replay and duplicate checksums', () => {
    const s = signer(), other = signer(), data = Buffer.from(`${'a'.repeat(64)}  payload\n`), sig = s.signature(data);
    expect(verifyMinisign(data, sig, [s.key])).toBe('vibeke v0.3.0');
    expect(() => verifyMinisign(data, sig, [other.key])).toThrow();
    expect(() => verifyMinisign(data, sig.replace('v0.3.0', 'v0.4.0'), [s.key])).toThrow();
    expect(() => verifiedSums(data, sig, [s.key], '0.4.0')).toThrow();
    const duplicated = Buffer.concat([data, data]);
    expect(() => verifiedSums(duplicated, s.signature(duplicated), [s.key], '0.3.0')).toThrow('duplicate');
  });
  test('excludes drafts, prereleases, unsupported architectures and incomplete releases', async () => {
    const f = fixture();
    f.meta.prerelease = true;
    await expect(discoverRelease('darwin', 'arm64', [f.s.key], f.get)).rejects.toThrow();
    f.meta.prerelease = false; f.meta.draft = true;
    await expect(discoverRelease('darwin', 'arm64', [f.s.key], f.get)).rejects.toThrow();
    f.meta.draft = false;
    await expect(discoverRelease('linux', 'arm64', [f.s.key], f.get)).rejects.toThrow();
    f.meta.assets = f.meta.assets.filter((a) => a.name !== 'SHA256SUMS.minisig');
    await expect(discoverRelease('darwin', 'arm64', [f.s.key], f.get)).rejects.toThrow('incomplete');
  });
  test('older releases without signed channels offer manual installation', async () => {
    const f = fixture(); f.meta.assets = f.meta.assets.filter((a) => a.name !== 'latest-mac.yml');
    expect((await discoverRelease('darwin', 'arm64', [f.s.key], f.get)).channel).toBeNull();
  });
  test('native metadata cannot escape its release or use a web installer', async () => {
    const f = fixture(), r = await discoverRelease('darwin', 'arm64', [f.s.key], f.get);
    for (const name of ['https://evil.example/app.zip', '../app.zip', 'Vibeke-0.2.0-mac-arm64.zip']) {
      expect(() => checkedInfo({ ...r, channel: r.channel!.replace('Vibeke-0.3.0-mac-arm64.zip', name) })).toThrow();
    }
    expect(() => checkedInfo({ ...r, channel: `${r.channel}\npackages: {}\n` })).toThrow();
  });
  test('stable version ordering is numeric and never offers a downgrade', () => {
    expect(newer('0.10.0', '0.9.0')).toBe(true);
    expect(newer('0.3.0', '0.3.0')).toBe(false);
    expect(newer('0.2.0', '0.3.0')).toBe(false);
    expect(newer('0.3.0', '0.3.0-rc.1')).toBe(true);
    expect(() => newer('../bad', '0.3.0')).toThrow();
  });
});

const release: DesktopRelease = { version: '0.3.0', base: '', channelName: 'latest.yml', channel: 'signed', releaseUrl: 'https://example.com/release', downloadUrl: 'https://example.com/download', assets: [] };
function controller(options: { manualReason?: string; discover?: () => Promise<DesktopRelease>; download?: () => Promise<void>; install?: () => void } = {}) {
  const calls = { checks: 0, downloads: 0, installs: 0, before: 0 };
  const c = new UpdateController({ version: '0.2.0', manualReason: options.manualReason ?? null,
    discover: async () => { calls.checks++; return options.discover ? options.discover() : release; }, changed: () => {},
    beforeInstall: () => { calls.before++; }, installer: () => ({
      download: async (_r, progress) => { calls.downloads++; progress(42); await options.download?.(); },
      install: () => { calls.installs++; options.install?.(); },
    }),
  });
  return { c, calls };
}
describe('update lifecycle', () => {
  test('coalesces checks, requires download, waits for explicit restart and installs once', async () => {
    const { c, calls } = controller();
    await Promise.all([c.check(), c.check()]); expect(calls.checks).toBe(1);
    expect(c.snapshot().status).toBe('available'); expect(calls.downloads).toBe(0);
    expect(() => c.install()).toThrow();
    await Promise.all([c.download(), c.download()]); expect(calls.downloads).toBe(1);
    expect(c.snapshot().status).toBe('ready'); expect(calls.installs).toBe(0);
    await c.check(); expect(c.snapshot().status).toBe('ready');
    c.install(); c.install(); expect(calls.installs).toBe(1); expect(calls.before).toBe(1);
  });
  test('failed checks never claim the app is current; retry recovers', async () => {
    let fail = true;
    const { c } = controller({ discover: async () => { if (fail) throw new Error('offline'); return release; } });
    await c.check(); expect(c.snapshot().status).toBe('error');
    fail = false; await c.check(); expect(c.snapshot().status).toBe('available');
  });
  test('download failures cannot install and can be retried', async () => {
    let fail = true;
    const { c } = controller({ download: async () => { if (fail) throw new Error('interrupted'); } });
    await c.check(); await c.download(); expect(c.snapshot().status).toBe('error'); expect(() => c.install()).toThrow();
    fail = false; await c.download(); expect(c.snapshot().status).toBe('ready');
  });
  test('manual packages never start the native updater', async () => {
    const { c, calls } = controller({ manualReason: 'Install the new package' });
    await c.check(); await expect(c.download()).rejects.toThrow(); expect(calls.downloads).toBe(0);
  });
  test('native asynchronous install errors recover the app instead of leaving it quitting', async () => {
    let fail: ((e: Error) => void) | undefined;
    let restored = 0;
    const c = new UpdateController({ version: '0.2.0', manualReason: null, discover: async () => release,
      changed: () => {}, beforeInstall: () => {}, installFailed: () => { restored++; },
      installer: () => ({ download: async () => {}, install: (onError) => { fail = onError; } }),
    });
    await c.check(); await c.download(); c.install();
    fail!(new Error('Permission denied'));
    expect(c.snapshot().status).toBe('error'); expect(c.snapshot().message).toBe('Permission denied');
    expect(restored).toBe(1);
    await c.check(); expect(c.snapshot().status).toBe('available');
  });
  test('background offline failures preserve an available release and do not show an error badge', async () => {
    let fail = false;
    const { c } = controller({ discover: async () => { if (fail) throw new Error('offline'); return release; } });
    await c.check(); fail = true; await c.check(true);
    expect(c.snapshot().status).toBe('available');
    expect(updateLabel(c.snapshot())).toContain('Update available');
    expect(c.snapshot().message).toContain('offline');
    await c.download(); expect(c.snapshot().status).toBe('ready');
    const quiet = controller({ discover: async () => { throw new Error('offline'); } }).c;
    await quiet.check(true); expect(quiet.snapshot().status).toBe('idle');
    expect(updateLabel(quiet.snapshot())).toBeNull();
  });
  test('unsupported desktop platforms have an explanation without a failed-update badge', async () => {
    const { c } = controller({ discover: async () => { throw new UnsupportedUpdatePlatform('unsupported'); } });
    await c.check(); expect(c.snapshot().status).toBe('unsupported');
    expect(updateLabel(c.snapshot())).toBeNull();
  });
  test('sidebar reflects the actual operation', () => {
    expect(updateLabel({ status: 'up-to-date', currentVersion: '0.2.0' })).toBeNull();
    expect(updateLabel({ status: 'downloading', currentVersion: '0.2.0', progress: 42 })).toContain('42%');
    expect(updateLabel({ status: 'ready', currentVersion: '0.2.0' })).toBe('Restart and update');
  });
});
