import { describe, expect, test } from 'bun:test';
import type { RpcLink } from '../src/rpc';
import { TunnelClient } from '../src/tunnel';

/** A link whose `tunnel.open` answers only when the test says so. */
function slowLink() {
  const calls: { method: string; params: Record<string, unknown> | undefined }[] = [];
  let answer: ((v: unknown) => void) | null = null;
  const link: RpcLink = {
    closed: false,
    request<T>(method: string, params?: Record<string, unknown>): Promise<T> {
      calls.push({ method, params });
      if (method === 'tunnel.open') return new Promise<T>((r) => (answer = r as (v: unknown) => void));
      return Promise.resolve({} as T);
    },
    on: () => () => {},
    onClose: () => () => {},
  };
  return { link, calls, answer: (v: unknown) => answer?.(v) };
}

describe('tunnel client', () => {
  test('a stream that opens after dispose is closed on the host', async () => {
    const l = slowLink();
    const c = new TunnelClient(l.link);
    const opening = c.open({ preview: 'pv1' });
    c.dispose();
    l.answer({ stream: 's1' });
    await expect(opening).rejects.toThrow();
    expect(l.calls.some((x) => x.method === 'tunnel.close' && x.params?.stream === 's1')).toBe(true);
  });
});
