// Run against an assembled draft, or one platform's packaging output, before signing.
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { checkedInfo } from '../src/main/update-info';
import { RELEASE_REPO, versionParts } from '../src/main/update-release';

const dir = resolve(process.argv[2] ?? 'dist');
const at = process.argv.indexOf('--platform');
const platform = at < 0 ? null : process.argv[at + 1];
const channels = platform === 'mac' ? ['latest-mac.yml'] : platform === 'windows' ? ['latest.yml'] : platform === 'linux' ? ['latest-linux.yml'] : ['latest.yml', 'latest-mac.yml', 'latest-linux.yml'];
const assets = readdirSync(dir);
const expected = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')).version as string;
versionParts(expected);
for (const channelName of channels) {
  const release = { version: expected, base: `${RELEASE_REPO}/releases/download/v${expected}`, channelName,
    channel: readFileSync(join(dir, channelName), 'utf8'), assets, releaseUrl: '', downloadUrl: '' };
  const info = checkedInfo(release);
  for (const f of info.files) {
    const bytes = readFileSync(join(dir, f.url));
    if (createHash('sha512').update(bytes).digest('base64') !== f.sha512 || (f.size !== undefined && f.size !== bytes.length)) throw new Error(`Metadata does not match ${f.url}`);
    if (/\.(exe|zip)$/.test(f.url) && !assets.includes(`${f.url}.blockmap`)) throw new Error(`Missing blockmap for ${f.url}`);
  }
  if (channelName === 'latest-mac.yml') for (const arch of ['arm64', 'x64']) {
    if (!info.files.some((f) => f.url === `Vibeke-${expected}-mac-${arch}.zip`)) throw new Error(`macOS channel missing ${arch} ZIP`);
  }
  console.log(`${channelName}: v${expected}, ${info.files.length} verified files`);
}
