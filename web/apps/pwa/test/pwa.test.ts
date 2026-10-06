import { describe, expect, test } from 'bun:test';
import { detectPlatform } from '../src/detect';
import { openTarget, parsePushData, planNotification, safeHashUrl } from '../src/push-payload';
import { WsSocket, type WebSocketLike } from '../src/ws-socket';

const ASSETS = { icon: '/i.png', badge: '/b.png' };

describe('push payload (vk-gateway notify.rs shape)', () => {
  test('interaction push → notification with tag, renotify and deep link', () => {
    const p = planNotification(
      { title: 'Codex · samplehub needs approval', body: 'devbox', tag: 'vibeke:h1', url: '#/i/h1/int1', host: 'h1', count: 1, renotify: true },
      ASSETS,
    );
    expect(p.title).toBe('Codex · samplehub needs approval');
    expect(p.options).toEqual({ body: 'devbox', tag: 'vibeke:h1', renotify: true, data: { url: '#/i/h1/int1', host: 'h1' }, icon: '/i.png', badge: '/b.png' });
  });
  test('merged push opens the inbox; push.test payload without url/host still shows', () => {
    expect(planNotification({ title: '3 agents need you', body: 'devbox', tag: 'vibeke:h1', url: '#/inbox', count: 3, renotify: true }, ASSETS).options.data.url).toBe('#/inbox');
    const test_ = planNotification({ title: 'Vibeke', body: 'Test from devbox', tag: 'vibeke:h1:test' }, ASSETS);
    expect(test_.options.tag).toBe('vibeke:h1:test');
    expect(test_.options.renotify).toBe(false);
    expect(test_.options.data.url).toBe('#/inbox');
  });
  test('garbage payloads degrade to a visible generic notification', () => {
    expect(planNotification(null, ASSETS).title).toBe('Vibeke');
    expect(planNotification('plain text', ASSETS).options.body).toBe('plain text');
    expect(planNotification({ title: 5, tag: '' }, ASSETS)).toMatchObject({ title: 'Vibeke', options: { tag: 'vibeke' } });
  });
  test('only in-app hash routes are accepted as targets', () => {
    expect(safeHashUrl('#/r/h1/run_2')).toBe('#/r/h1/run_2');
    expect(safeHashUrl('#/i/h1/x?do=allow')).toBe('#/i/h1/x?do=allow');
    expect(safeHashUrl('https://evil.example/')).toBe('#/inbox');
    expect(safeHashUrl('javascript:alert(1)')).toBe('#/inbox');
    expect(safeHashUrl('#/<script>')).toBe('#/inbox');
    expect(openTarget('https://app.example/', '#/r/h/x')).toBe('https://app.example/#/r/h/x');
    expect(openTarget('https://app.example/sub', '#/')).toBe('https://app.example/sub/#/');
  });
  test('push data parsing falls back to text', () => {
    expect(parsePushData({ json: () => ({ a: 1 }), text: () => '' })).toEqual({ a: 1 });
    expect(parsePushData({ json: () => { throw new Error('x'); }, text: () => 'hi' })).toBe('hi');
    expect(parsePushData(null)).toEqual({});
  });
});

describe('platform detection', () => {
  test('labels', () => {
    expect(detectPlatform('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)', 'iPhone', 5)).toMatchObject({ name: 'iOS', ios: true });
    expect(detectPlatform('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', 'MacIntel', 5)).toMatchObject({ name: 'iPadOS', ios: true });
    expect(detectPlatform('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', 'MacIntel', 0)).toMatchObject({ name: 'macOS', ios: false });
    expect(detectPlatform('Mozilla/5.0 (Linux; Android 15; Pixel) Mobile', 'Linux armv8l', 5)).toMatchObject({ name: 'Android', device: 'Android phone' });
  });
});

class FakeWs implements WebSocketLike {
  readyState = 0;
  binaryType = 'blob';
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  sent: unknown[] = [];
  closed: [number?, string?] | null = null;
  send(d: unknown) {
    this.sent.push(d);
  }
  close(code?: number, reason?: string) {
    this.closed = [code, reason];
  }
}

describe('WebSocket adapter', () => {
  test('preserves text vs binary and reports close once', async () => {
    const ws = new FakeWs();
    const s = new WsSocket(ws);
    expect(ws.binaryType).toBe('arraybuffer');
    const got: (string | Uint8Array)[] = [];
    const closes: number[] = [];
    s.onmessage = (d) => got.push(d);
    s.onclose = (c) => closes.push(c);
    ws.onopen!({});
    expect(s.state).toBe('open');
    ws.onmessage!({ data: 'hello' });
    ws.onmessage!({ data: new Uint8Array([1, 2]).buffer });
    expect(got[0]).toBe('hello');
    expect(got[1]).toEqual(new Uint8Array([1, 2]));
    s.send(new Uint8Array([3]));
    expect(ws.sent.length).toBe(1);
    ws.onerror!({});
    ws.onclose!({ code: 4404, reason: 'host_offline' });
    await Promise.resolve();
    expect(closes).toEqual([4404]);
    expect(s.state).toBe('closed');
    expect(() => s.send('x')).toThrow();
  });
  test('local close uses a code the browser accepts', () => {
    const ws = new FakeWs();
    const s = new WsSocket(ws);
    ws.onopen!({});
    s.close(1006, 'x');
    expect(ws.closed).toEqual([1000, 'x']);
  });
});
