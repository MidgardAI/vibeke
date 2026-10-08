import { expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DraftStore } from '../src/main/drafts';
import { syncDraftHosts } from '../src/main/draft-lifecycle';
import type { Engine } from '../src/main/engine';

test('partial startup and re-pair patches cannot delete other hosts drafts', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'vk-draft-hosts-'));
  const safe = { isEncryptionAvailable: () => true, encryptString: (s: string) => Buffer.from(s), decryptString: (b: Buffer) => b.toString() };
  const drafts = new DraftStore(dir, safe, 'darwin', 1);
  try {
    await Promise.all([drafts.set('a', 'p', 'one'), drafts.set('b', 'p', 'two')]);
    let hosts = ['a'];
    let started!: () => void;
    const start = new Promise<void>((resolve) => { started = resolve; });
    const engine = { start: () => start, snapshot: () => hosts.map((id) => ({ record: { host_id: id } })) as ReturnType<Engine['snapshot']> };
    const initial = syncDraftHosts(engine, drafts);
    hosts = ['a', 'b']; started(); await initial;
    expect(await drafts.get('b', 'p')).toBe('two');
    hosts = ['a'];
    const rePair = syncDraftHosts(engine, drafts);
    hosts = ['a', 'b']; await rePair;
    expect(await drafts.get('b', 'p')).toBe('two');
    hosts = ['a']; await syncDraftHosts(engine, drafts); await drafts.flush();
    expect(await drafts.get('b', 'p')).toBe('');
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
