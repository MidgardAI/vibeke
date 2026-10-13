import { describe, expect, test } from 'bun:test';
import * as b64 from '../src/b64';
import { x25519Public } from '../src/keys';
import { previewAuthority, previewPath, ReviewConnection, reviewKey, reviewRecord, type ReviewRecord } from '../src/review';
import { TunnelClient } from '../src/tunnel';
import type { RpcLink } from '../src/rpc';
import { flush, serveGateway, testPlatform } from './helpers';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 11 + i * 3) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);

const record = (): ReviewRecord =>
  reviewRecord(
    { host_id: 'hostid', relay: 'wss://relay.example', hk: b64.encode(x25519Public(HOST)), device_id: 'd1', name: 'devbox', scope: 'view' },
    { pid: 'pid-1', share: { sid: 'ABCDEFGHIJKLMNOP', preview: { id: 'pv_1', label: 'v4' }, notes: 'held' } },
  );

const info = { preview: { id: 'pv_1', handle: 'v4', label: 'App', url: 'http://localhost:5173/app', status: 'live', port: 5173, path: '/app' }, owner: 'alice', notes: 'held', sid: 'abcdefghijklmnop', expires_at: 1_900_000_000 };

describe('review records', () => {
  test('built from the link, keyed by host and pid', () => {
    const r = record();
    expect(r).toMatchObject({ kind: 'share', pid: 'pid-1', sid: 'abcdefghijklmnop', preview: { id: 'pv_1', label: 'v4' }, notes: 'held' });
    expect(reviewKey(r.host_id, r.pid)).toBe('hostid/pid-1');
    expect(() => reviewRecord(r, { pid: 'x', share: {} })).toThrow();
  });
  test('preview authority and path', () => {
    expect(previewAuthority(info as never)).toBe('localhost:5173');
    expect(previewAuthority({ ...info, preview: { ...info.preview, port: undefined, url: 'http://127.0.0.1:3000/' } } as never)).toBe('127.0.0.1:3000');
    expect(previewPath(info as never)).toBe('/app');
    expect(previewPath({ ...info, preview: { ...info.preview, path: undefined, url: 'http://localhost:1/x?y=1' } } as never)).toBe('/x?y=1');
  });
});

describe('review connection', () => {
  test('hello, review.info, then notes polling; never the dashboard or events', async () => {
    const calls: string[] = [];
    let state = 'open';
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        handle(method, params) {
          calls.push(method);
          switch (method) {
            case 'hello':
              return { host_name: 'devbox', device_id: 'd1', scope: 'view', server_version: '1', features: [], kind: 'share', review: { preview: 'pv_1' } };
            case 'review.info':
              return info;
            case 'relay.ticket':
              return { ticket: 'new.ticket', exp: 1_900_000_000 };
            case 'browser.annotations':
              expect(params).toEqual({ preview: 'pv_1' });
              return {
                draft: { id: 'dr1', target: { preview: 'pv_1' }, url: info.preview.url, notes: [], state, updated_at_ms: 1 },
                drafts: [{ id: 'dr1', target: { preview: 'pv_1' }, url: info.preview.url, notes: [], state, updated_at_ms: 1 }],
              };
            default:
              throw { code: -32601, message: 'refused', data: { kind: 'forbidden' } };
          }
        },
      }),
    );
    const saved: ReviewRecord[] = [];
    const c = new ReviewConnection(record(), { platform, devicePrivate: DEV, client: { client: 'test', version: '1' }, persist: async (r) => void saved.push(r) });
    c.start();
    await platform.clock.advance(10);
    expect(c.getSnapshot().status).toBe('online');
    expect(c.getSnapshot().review).toMatchObject({ owner: 'alice', notes: 'held' });
    expect(c.getSnapshot().draft?.id).toBe('dr1');
    expect(c.streamLink()).not.toBeNull();

    state = 'held';
    await platform.clock.advance(3000);
    expect(c.getSnapshot().draft).toBeNull();
    expect(c.getSnapshot().sent.map((d) => [d.id, d.state])).toEqual([['dr1', 'held']]);
    expect(saved.some((r) => r.sent?.includes('dr1'))).toBe(true);
    expect(saved.some((r) => r.ticket === 'new.ticket')).toBe(true);

    expect(calls).not.toContain('dashboard.get');
    expect(calls).not.toContain('events.subscribe');
    expect(calls.slice(0, 2)).toEqual(['hello', 'review.info']);
    c.stop();
    await flush();
  });
});

describe('tunnel client', () => {
  test('routes data, acks in batches, and closes', async () => {
    const handlers = new Map<string, (p: unknown) => void>();
    const requests: [string, Record<string, unknown>][] = [];
    const link: RpcLink = {
      closed: false,
      async request(method: string, params: Record<string, unknown> = {}) {
        requests.push([method, params]);
        if (method === 'tunnel.open') {
          // data for the stream may arrive before the open response
          handlers.get('tunnel.data')?.({ stream: 's1', data_b64: b64.encode(new Uint8Array([1, 2])) });
          return { stream: 's1' } as never;
        }
        return {} as never;
      },
      on(method, cb) {
        handlers.set(method, cb as (p: unknown) => void);
        return () => handlers.delete(method);
      },
      onClose() {
        return () => {};
      },
    };
    const t = new TunnelClient(link);
    const conn = await t.open({ preview: 'pv_1' });
    expect(requests[0]).toEqual(['tunnel.open', { preview: 'pv_1' }]);
    const got: number[] = [];
    const ends: string[] = [];
    conn.listen({ data: (b) => got.push(...b), end: () => ends.push('end'), close: (r) => ends.push(r) });
    expect(got).toEqual([1, 2]);
    handlers.get('tunnel.data')!({ stream: 's1', data_b64: b64.encode(new Uint8Array(70 * 1024)) });
    expect(requests.some(([m, p]) => m === 'tunnel.ack' && (p.bytes as number) >= 64 * 1024)).toBe(true);
    await conn.write(new Uint8Array(100 * 1024));
    expect(requests.filter(([m]) => m === 'tunnel.send')).toHaveLength(2);
    handlers.get('tunnel.closed')!({ stream: 's1', reason: 'eof' });
    handlers.get('tunnel.closed')!({ stream: 's1', reason: 'closed' });
    expect(ends).toEqual(['end', 'closed']);
    expect(t.size).toBe(0);
  });
});
