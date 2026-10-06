import { describe, expect, test } from 'bun:test';
import { HandoffCancelled, exportHandoff, handoffSummary, isHandoffBusy, transferHandoff, type ApiCaller, type ExportedHandoff } from '../src/handoff';
import { RpcError } from '../src/rpc';
import type { HandoffManifest } from '../src/model';

const manifest = (p: Partial<HandoffManifest> = {}): HandoffManifest => ({
  v: 1,
  source_host: 'src',
  repo_name: 'repo',
  origin: 'git@github.com:a/repo.git',
  branch: 'feat',
  head: 'abc',
  bundle: 'thin',
  cwd_rel: '',
  source_cwd: '/r',
  source_root: '/r',
  harness: 'claude',
  session_id: 's1',
  resume_args: ['--resume', 's1'],
  transcript_rel: 'projects/-r/s1.jsonl',
  last_message: 'done',
  untracked: ['a.txt'],
  skipped: [
    { path: '.env', reason: 'secret' },
    { path: 'big.bin', reason: 'larger than 5 MiB' },
  ],
  redactions: 3,
  created_at: 0,
  ...p,
});

/** Source holds `data`; dest appends like the gateway (offset must match what it has). */
function fakes(data: Uint8Array) {
  const log: [string, string, any][] = [];
  let received = new Uint8Array(0);
  const b64 = (u: Uint8Array) => Buffer.from(u).toString('base64');
  const source: ApiCaller = {
    request: (async (method: string, p: any) => {
      log.push(['src', method, p]);
      if (method === 'handoff.read') {
        const slice = data.subarray(p.offset, p.offset + p.len);
        return { data_b64: b64(slice), eof: p.offset + slice.length >= data.length, size: data.length };
      }
      return {};
    }) as ApiCaller['request'],
  };
  const dest: ApiCaller = {
    request: (async (method: string, p: any) => {
      log.push(['dst', method, p]);
      if (method === 'handoff.begin') return { id: 'd1' };
      if (method === 'handoff.write') {
        if (p.offset !== received.length) throw new Error('offset');
        const add = Buffer.from(p.data_b64, 'base64');
        const n = new Uint8Array(received.length + add.length);
        n.set(received);
        n.set(add, received.length);
        received = n;
        return { received: received.length };
      }
      return {};
    }) as ApiCaller['request'],
  };
  return { source, dest, log, got: () => received };
}

const exported = (data: Uint8Array): ExportedHandoff => ({ id: 's1', size: data.length, sha256: 'x'.repeat(64), manifest: manifest() });

describe('handoff transfer', () => {
  test('copies in chunks with progress and returns the destination id', async () => {
    const data = Uint8Array.from({ length: 10_000 }, (_, i) => i & 0xff);
    const f = fakes(data);
    const progress: number[] = [];
    const id = await transferHandoff({ source: f.source, dest: f.dest, exported: exported(data), chunk: 4096, onProgress: (s) => progress.push(s) });
    expect(id).toBe('d1');
    expect(f.got()).toEqual(data);
    expect(progress).toEqual([0, 4096, 8192, 10_000]);
    expect(f.log.filter((l) => l[1] === 'handoff.read').map((l) => l[2].offset)).toEqual([0, 4096, 8192]);
    expect(f.log[0]).toEqual(['dst', 'handoff.begin', { manifest: manifest(), size: 10_000, sha256: 'x'.repeat(64) }]);
  });

  test('cancel discards both ends', async () => {
    const data = new Uint8Array(10_000);
    const f = fakes(data);
    const token = { cancelled: false };
    const p = transferHandoff({
      source: f.source,
      dest: f.dest,
      exported: exported(data),
      chunk: 4096,
      cancel: token,
      onProgress: (s) => {
        if (s >= 4096) token.cancelled = true;
      },
    });
    await expect(p).rejects.toBeInstanceOf(HandoffCancelled);
    const discards = f.log.filter((l) => l[1] === 'handoff.discard').map((l) => [l[0], l[2].id]);
    expect(discards.sort()).toEqual([
      ['dst', 'd1'],
      ['src', 's1'],
    ]);
  });

  test('a failed write discards the destination copy but keeps the source export', async () => {
    const data = new Uint8Array(10_000);
    const f = fakes(data);
    const dest: ApiCaller = {
      request: (async (m: string, p: any) => {
        if (m === 'handoff.write' && p.offset > 0) throw new Error('boom');
        return f.dest.request(m as any, p);
      }) as ApiCaller['request'],
    };
    await expect(transferHandoff({ source: f.source, dest, exported: exported(data), chunk: 4096 })).rejects.toThrow('boom');
    expect(f.log.filter((l) => l[1] === 'handoff.discard').map((l) => l[0])).toEqual(['dst']);
  });

  test('summary: resumable, secrets split from other skips', () => {
    const s = handoffSummary(manifest());
    expect(s.resumable).toBe(true);
    expect(s.secrets).toEqual(['.env']);
    expect(s.otherSkipped).toEqual([{ path: 'big.bin', reason: 'larger than 5 MiB' }]);
    expect(s.redactions).toBe(3);
    expect(handoffSummary(manifest({ transcript_rel: null })).resumable).toBe(false);
  });
});

describe('handoff export when the agent is working', () => {
  const busy = () => new RpcError('handoff.export', { code: -32000, message: 'the agent is working; wait for it to finish or pass interrupt: true', data: { kind: 'busy' } });
  function source(working: { now: boolean }) {
    const calls: any[] = [];
    const caller: ApiCaller = {
      request: (async (method: string, p: any) => {
        calls.push([method, p]);
        if (method !== 'handoff.export') return {};
        if (working.now && !p.interrupt) throw busy();
        working.now = false;
        return { id: 'x1', size: 3, sha256: 'h', manifest: manifest() };
      }) as ApiCaller['request'],
    };
    return { caller, calls };
  }

  test('busy → offer interrupt → export with interrupt: true', async () => {
    const { caller, calls } = source({ now: true });
    expect(await exportHandoff(caller, 'p1')).toEqual({ k: 'busy' });
    const r = await exportHandoff(caller, 'p1', true);
    expect(r.k).toBe('exported');
    expect(calls.map((c) => c[1])).toEqual([{ pane: 'p1' }, { pane: 'p1', interrupt: true }]);
  });

  test('idle agent exports directly; other errors propagate', async () => {
    const { caller } = source({ now: false });
    expect((await exportHandoff(caller, 'p1')).k).toBe('exported');
    const failing: ApiCaller = { request: (async () => { throw new RpcError('handoff.export', { code: -1, message: 'nope', data: { kind: 'not_found' } }); }) as ApiCaller['request'] };
    await expect(exportHandoff(failing, 'p1')).rejects.toThrow('nope');
    // With interrupt already requested, busy is an error (never loops back to the offer).
    const stillBusy: ApiCaller = { request: (async () => { throw busy(); }) as ApiCaller['request'] };
    await expect(exportHandoff(stillBusy, 'p1', true)).rejects.toThrow('working');
  });

  test('busy kinds', () => {
    expect(isHandoffBusy(busy())).toBe(true);
    expect(isHandoffBusy(new RpcError('handoff.export', { code: -1, message: 'the agent is working', data: { kind: 'conflict' } }))).toBe(true);
    expect(isHandoffBusy(new RpcError('handoff.export', { code: -1, message: 'repo conflict', data: { kind: 'conflict' } }))).toBe(false);
    expect(isHandoffBusy(new Error('busy'))).toBe(false);
  });
});
