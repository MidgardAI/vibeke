// Public release discovery and minisign verification, independent of Electron. The exact
// channel bytes are authenticated BEFORE electron-updater parses or uses them.
import { createHash, createPublicKey, verify } from 'node:crypto';

export const RELEASE_REPO = 'https://github.com/MidgardAI/vibeke';
export const UPDATE_FEED = `${RELEASE_REPO}/releases/latest/download`;
export const RELEASE_API = 'https://api.github.com/repos/MidgardAI/vibeke/releases/latest';
export type FetchBytes = (url: string, limit?: number) => Promise<Buffer>;
export interface DesktopRelease {
  version: string;
  base: string;
  releaseUrl: string;
  downloadUrl: string;
  channelName: string;
  channel: string | null;
  assets: string[];
}

export function versionParts(s: string): number[] {
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(s)) throw new Error('Invalid stable release version');
  const parts = s.split('.').map(Number);
  if (parts.some((n) => !Number.isSafeInteger(n))) throw new Error('Invalid stable release version');
  return parts;
}
export function newer(a: string, b: string): boolean {
  const av = versionParts(a), bv = versionParts(b.split(/[+-]/)[0]!);
  for (let i = 0; i < 3; i++) if (av[i] !== bv[i]) return av[i]! > bv[i]!;
  return b.includes('-');
}
function b64(s: string, size: number): Buffer {
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(s)) throw new Error('Invalid minisign encoding');
  const b = Buffer.from(s, 'base64');
  if (b.length !== size || b.toString('base64') !== s) throw new Error('Invalid minisign length');
  return b;
}
export function verifyMinisign(data: Buffer, signature: string, keys: readonly string[]): string {
  const lines = signature.trim().split(/\r?\n/).filter(Boolean);
  if (lines[0]?.startsWith('untrusted comment:')) lines.shift();
  if (lines.length !== 3 || !lines[1]?.startsWith('trusted comment: ')) throw new Error('Malformed release signature');
  const raw = b64(lines[0]!, 74), global = b64(lines[2]!, 64);
  const alg = raw.subarray(0, 2).toString();
  if (alg !== 'ED' && alg !== 'Ed') throw new Error('Unsupported signature algorithm');
  const publicKey = keys.map((k) => b64(k, 42)).find((k) => k.subarray(0, 2).toString() === 'Ed' && k.subarray(2, 10).equals(raw.subarray(2, 10)));
  if (!publicKey) throw new Error('Release signature uses an untrusted key');
  const key = createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), publicKey.subarray(10)]), format: 'der', type: 'spki' });
  const sig = raw.subarray(10), comment = lines[1]!.slice('trusted comment: '.length);
  if (!verify(null, alg === 'ED' ? createHash('blake2b512').update(data).digest() : data, key, sig)
      || !verify(null, Buffer.concat([sig, Buffer.from(comment)]), key, global)) throw new Error('Release signature verification failed');
  return comment;
}
export function verifiedSums(data: Buffer, signature: string, keys: readonly string[], version: string): Map<string, string> {
  if (verifyMinisign(data, signature, keys) !== `vibeke v${version}`) throw new Error('Signature names a different release version');
  const sums = new Map<string, string>();
  for (const line of data.toString().trim().split('\n')) {
    const m = /^([a-f0-9]{64})  ([A-Za-z0-9._-]+)$/.exec(line);
    if (!m || sums.has(m[2]!)) throw new Error('Malformed or duplicate release checksum');
    sums.set(m[2]!, m[1]!);
  }
  return sums;
}

export const fetchBytes: FetchBytes = async (url, limit = 2 * 1024 * 1024) => {
  let target = new URL(url);
  const signal = AbortSignal.timeout(30_000);
  for (let hop = 0; hop < 6; hop++) {
    if (target.protocol !== 'https:' || target.username || target.password) throw new Error('Update downloads require HTTPS');
    const response = await fetch(target, { redirect: 'manual', signal, headers: { 'User-Agent': 'Vibeke-Updater', Accept: target.hostname === 'api.github.com' ? 'application/vnd.github+json' : 'application/octet-stream' } });
    if ([301, 302, 303, 307, 308].includes(response.status)) {
      const location = response.headers.get('location');
      await response.body?.cancel();
      if (!location) throw new Error('Invalid release redirect');
      target = new URL(location, target);
      continue;
    }
    if (!response.ok || !response.body) throw new Error(`Release check failed (HTTP ${response.status})`);
    if (Number(response.headers.get('content-length') ?? 0) > limit) { await response.body.cancel(); throw new Error('Release metadata is too large'); }
    const reader = response.body.getReader(), chunks: Buffer[] = [];
    let total = 0;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return Buffer.concat(chunks);
        total += value.byteLength;
        if (total > limit) throw new Error('Release metadata is too large');
        chunks.push(Buffer.from(value));
      }
    } finally { await reader.cancel(); }
  }
  throw new Error('Too many update redirects');
};

export async function discoverRelease(platform: string, arch: string, keys: readonly string[], get: FetchBytes = fetchBytes, linuxDeb = false): Promise<DesktopRelease> {
  const release = JSON.parse((await get(RELEASE_API)).toString());
  if (release.draft !== false || release.prerelease !== false || !Array.isArray(release.assets)) throw new Error('No published stable release');
  const version = typeof release.tag_name === 'string' && release.tag_name.startsWith('v') ? release.tag_name.slice(1) : '';
  versionParts(version);
  const base = `${RELEASE_REPO}/releases/download/v${version}`;
  const assets = release.assets.filter((a: { name?: unknown; browser_download_url?: unknown }) => typeof a.name === 'string' && /^[A-Za-z0-9._-]+$/.test(a.name) && a.browser_download_url === `${base}/${a.name}`).map((a: { name: string }) => a.name) as string[];
  const name = platform === 'darwin' && ['arm64', 'x64'].includes(arch) ? `Vibeke-${version}-mac-${arch}.dmg`
    : platform === 'win32' && arch === 'x64' ? `Vibeke-${version}-win-x64.exe`
    : platform === 'linux' && arch === 'x64' ? (linuxDeb ? `Vibeke-${version}-linux-amd64.deb` : `Vibeke-${version}-linux-x86_64.AppImage`) : '';
  if (!name || !assets.includes(name)) throw new Error('This release has no desktop download for your platform');
  for (const required of ['SHA256SUMS', 'SHA256SUMS.minisig']) if (!assets.includes(required)) throw new Error(`Release is incomplete: missing ${required}`);
  const [data, signature] = await Promise.all([get(`${base}/SHA256SUMS`), get(`${base}/SHA256SUMS.minisig`, 4096)]);
  const sums = verifiedSums(data, signature.toString(), keys, version);
  if (!sums.has(name)) throw new Error('Desktop download is not covered by the release signature');
  const channelName = platform === 'darwin' ? 'latest-mac.yml' : platform === 'win32' ? 'latest.yml' : 'latest-linux.yml';
  let channel: string | null = null;
  if (assets.includes(channelName) && sums.has(channelName)) {
    const bytes = await get(`${base}/${channelName}`);
    if (createHash('sha256').update(bytes).digest('hex') !== sums.get(channelName)) throw new Error('Update metadata checksum mismatch');
    channel = bytes.toString();
  }
  return { version, base, releaseUrl: `${RELEASE_REPO}/releases/tag/v${version}`, downloadUrl: `${base}/${name}`, channelName, channel, assets };
}
