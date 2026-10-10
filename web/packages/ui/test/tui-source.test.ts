import { test, expect } from 'bun:test';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { sourceInputs, tuiSourceDigest } from '../../../scripts/tui-source';

test('WASM fingerprint follows target dependencies and ignores unrelated commits', () => {
  const root = mkdtempSync(join(tmpdir(), 'vibeke-tui-source-'));
  try {
    execFileSync('git', ['init', '-q', root]);
    const write = (file: string, data: string) => writeFileSync(join(root, file), data);
    for (const name of ['vk-tui', 'vk-proto', 'vk-server']) {
      mkdirSync(join(root, 'crates', name), { recursive: true });
      write(`crates/${name}/Cargo.toml`, name);
      write(`crates/${name}/lib.rs`, 'original');
    }
    const inputs = sourceInputs({
      packages: ['vk-tui', 'vk-proto', 'vk-server'].map((name) => ({ id: name, name, manifest_path: join(root, 'crates', name, 'Cargo.toml'), source: null })),
      resolve: { nodes: [{ id: 'vk-tui', deps: [
        { pkg: 'vk-proto', dep_kinds: [{ kind: null }] },
        { pkg: 'vk-server', dep_kinds: [{ kind: 'dev' }] },
      ] }] },
    }, root);
    expect(inputs).toContain('crates/vk-proto');
    expect(inputs).not.toContain('crates/vk-server');
    execFileSync('git', ['add', '.'], { cwd: root });
    execFileSync('git', ['-c', 'user.name=Test', '-c', 'user.email=test@example.com', '-c', 'commit.gpgsign=false', 'commit', '-qm', 'Initial'], { cwd: root });
    const original = tuiSourceDigest(inputs, root);
    write('README.md', 'Documentation');
    write('crates/vk-server/lib.rs', 'server change');
    execFileSync('git', ['add', '.'], { cwd: root });
    execFileSync('git', ['-c', 'user.name=Test', '-c', 'user.email=test@example.com', '-c', 'commit.gpgsign=false', 'commit', '-qm', 'Unrelated'], { cwd: root });
    expect(tuiSourceDigest(inputs, root)).toBe(original);
    write('crates/vk-proto/lib.rs', 'protocol change');
    expect(tuiSourceDigest(inputs, root)).not.toBe(original);
    write('crates/vk-proto/lib.rs', 'original');
    write('crates/vk-tui/new.rs', 'new module');
    expect(tuiSourceDigest(inputs, root)).not.toBe(original);
    rmSync(join(root, 'crates/vk-tui/new.rs'));
    rmSync(join(root, 'crates/vk-tui/lib.rs'));
    expect(tuiSourceDigest(inputs, root)).not.toBe(original);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
