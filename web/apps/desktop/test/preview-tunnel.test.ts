import { describe, expect, test } from 'bun:test';
import type { RpcLink } from '@vibeke/core';
import { TunnelMux } from '../src/main/preview-tunnel';

/** An in-memory `RpcLink`: records requests, lets the test answer them and push notifications. */
class FakeLink implements RpcLink {
  closed = false;
  calls: { method: string; params: Record<string, unknown> }[] = [];
  private handlers = new Map<string, Set<(p: unknown, m: string) => void>>();
  private closers = new Set<() => void>();
  /** Answer for a method (default `{}`); a function may delay or fail. */
  answer: (method: string, params: Record<string, unknown>) => Promise<unknown> = async (m) => (m === 'tunnel.open' ? { stream: 's1' } : {});

  request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    this.calls.push({ method, params });
    return this.answer(method, params) as Promise<T>;
  }
  on(method: string, cb: (p: unknown, m: string) => void): () => void {
    let set = this.handlers.get(method);
    if (!set) this.handlers.set(method, (set = new Set()));
    set.add(cb);
    return () => set.delete(cb);
  }
  onClose(cb: () => void): () => void {
    this.closers.add(cb);
    return () => this.closers.delete(cb);
  }
  notify(method: string, params: unknown): void {
    for (const cb of this.handlers.get(method) ?? []) cb(params, method);
  }
  close(): void {
    this.closed = true;
    for (const cb of this.closers) cb();
  }
  sent(method: string) {
    return this.calls.filter((c) => c.method === method).map((c) => c.params);
  }
}

const tick = () => new Promise((r) => setTimeout(r, 5));

describe('tunnel streams', () => {
  test('open passes machine, host and port', async () => {
    const link = new FakeLink();
    const s = await new TunnelMux(link).open({ machine: 'devbox', host: 'localhost', port: 5173 });
    expect(s.id).toBe('s1');
    expect(link.sent('tunnel.open')).toEqual([{ machine: 'devbox', host: 'localhost', port: 5173 }]);
    s.destroy();
  });

  test('writes are split into 64 KiB sends, one at a time, as unpadded base64url', async () => {
    const link = new FakeLink();
    let inFlight = 0;
    let maxInFlight = 0;
    link.answer = async (m) => {
      if (m === 'tunnel.open') return { stream: 's1' };
      if (m === 'tunnel.send') {
        inFlight++;
        maxInFlight = Math.max(maxInFlight, inFlight);
        await tick();
        inFlight--;
      }
      return {};
    };
    const s = await new TunnelMux(link).open({ host: 'localhost', port: 80 });
    const data = Buffer.alloc(150 * 1024, 0xfb);
    await new Promise<void>((r) => s.write(data, () => r()));
    const sends = link.sent('tunnel.send');
    expect(sends.length).toBe(3);
    expect(maxInFlight).toBe(1);
    const joined = Buffer.concat(sends.map((p) => Buffer.from(p.data_b64 as string, 'base64url')));
    expect(joined.equals(data)).toBe(true);
    expect((sends[0]!.data_b64 as string).includes('=')).toBe(false);
    expect((sends[0]!.data_b64 as string)).toMatch(/^[A-Za-z0-9_-]+$/);
    await new Promise<void>((r) => s.end(() => r()));
    expect(link.sent('tunnel.send').at(-1)).toEqual({ stream: 's1', eof: true });
    s.destroy();
  });

  test('data is delivered in order and acknowledged as it is consumed', async () => {
    const link = new FakeLink();
    const s = await new TunnelMux(link).open({ host: 'localhost', port: 80 });
    const got: Buffer[] = [];
    s.on('data', (d: Buffer) => got.push(d));
    const chunk = Buffer.alloc(48 * 1024, 7);
    for (let i = 0; i < 4; i++) link.notify('tunnel.data', { stream: 's1', data_b64: chunk.toString('base64url') });
    await tick();
    expect(Buffer.concat(got).length).toBe(4 * chunk.length);
    const acked = link.sent('tunnel.ack').reduce((n, p) => n + (p.bytes as number), 0);
    expect(acked).toBeGreaterThanOrEqual(64 * 1024);
    expect(acked).toBeLessThanOrEqual(4 * chunk.length);
    s.destroy();
  });

  test('eof ends only the reading side; a later closed ends the stream', async () => {
    const link = new FakeLink();
    const s = await new TunnelMux(link).open({ host: 'localhost', port: 80 });
    let ended = false;
    let closed = false;
    s.on('end', () => (ended = true));
    s.on('close', () => (closed = true));
    s.resume();
    link.notify('tunnel.closed', { stream: 's1', reason: 'eof' });
    await tick();
    expect(ended).toBe(true);
    expect(closed).toBe(false);
    await new Promise<void>((r) => s.write(Buffer.from('still writing'), () => r()));
    expect(link.sent('tunnel.send').length).toBe(1);
    link.notify('tunnel.closed', { stream: 's1', reason: 'closed' });
    await tick();
    expect(closed).toBe(true);
  });

  test('error and a dropped connection destroy the stream', async () => {
    const link = new FakeLink();
    link.answer = async (m) => (m === 'tunnel.open' ? { stream: `s${link.sent('tunnel.open').length}` } : {});
    const mux = new TunnelMux(link);
    const a = await mux.open({ host: 'localhost', port: 80 });
    const b = await mux.open({ host: 'localhost', port: 81 });
    const errs: string[] = [];
    a.on('error', (e) => errs.push(e.message));
    b.on('error', (e) => errs.push(e.message));
    link.notify('tunnel.closed', { stream: a.id, reason: 'error' });
    await tick();
    expect(a.destroyed).toBe(true);
    link.close();
    await tick();
    expect(b.destroyed).toBe(true);
    expect(errs).toEqual(['tunnel stream error', 'host disconnected']);
    await expect(mux.open({ host: 'localhost', port: 82 })).rejects.toThrow('host disconnected');
  });

  test('destroying a stream sends tunnel.close', async () => {
    const link = new FakeLink();
    const s = await new TunnelMux(link).open({ host: 'localhost', port: 80 });
    s.destroy();
    await tick();
    expect(link.sent('tunnel.close')).toEqual([{ stream: 's1' }]);
  });
});
