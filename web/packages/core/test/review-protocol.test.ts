import { describe, expect, test } from 'bun:test';
import { parseBridgeMessage, parsePageMessage, parseWorkerMessage, REVIEW_PROTO, reviewContentOrigin } from '../src/review-protocol';

const CO = 'https://abcdefghijklmnop.p.vibeke.dev';
const m = (o: Record<string, unknown>) => ({ p: REVIEW_PROTO, ...o });

describe('content origin', () => {
  test('derived from the app origin', () => {
    expect(reviewContentOrigin('https://app.vibeke.dev', 'ABCDEFGHIJKLMNOP')).toBe(CO);
    expect(reviewContentOrigin('https://vibeke.example.com:8443', 'abcdefghijklmnop')).toBe('https://abcdefghijklmnop.p.example.com:8443');
    expect(reviewContentOrigin('http://localhost:5173', 'abcdefghijklmnop')).toBeNull();
    expect(reviewContentOrigin('https://10.0.0.1', 'abcdefghijklmnop')).toBeNull();
    expect(reviewContentOrigin('https://app.vibeke.dev', 'bad')).toBeNull();
  });
  test('pattern override', () => {
    expect(reviewContentOrigin('http://localhost:5173', 'abcdefghijklmnop', 'http://{sid}.p.localhost:5174')).toBe('http://abcdefghijklmnop.p.localhost:5174');
    expect(reviewContentOrigin('http://localhost:5173', 'abcdefghijklmnop', 'http://evil.com/{sid}')).toBeNull();
    expect(reviewContentOrigin('http://localhost:5173', 'abcdefghijklmnop', 'http://fixed.example')).toBeNull();
  });
});

describe('bridge messages', () => {
  test('only ready and channel', () => {
    expect(parseBridgeMessage(m({ t: 'ready' }))).toEqual({ t: 'ready' });
    expect(parseBridgeMessage(m({ t: 'channel' }))).toEqual({ t: 'channel' });
    expect(parseBridgeMessage({ t: 'ready' })).toBeNull();
    expect(parseBridgeMessage(m({ t: 'ready', extra: 1 }))).toBeNull();
    expect(parseBridgeMessage(m({ t: 'fetch' }))).toBeNull();
  });
});

describe('worker messages', () => {
  const fetch = m({ t: 'fetch', id: 1, method: 'GET', url: `${CO}/app?x=1`, headers: [['Accept', 'text/html']], body: null, nav: true });
  test('a valid fetch', () => {
    expect(parseWorkerMessage(fetch, CO)).toEqual({ t: 'fetch', id: 1, method: 'GET', url: `${CO}/app?x=1`, headers: [['Accept', 'text/html']], body: null, nav: true });
    expect(parseWorkerMessage(m({ ...fetch, method: 'POST', body: new ArrayBuffer(3) }), CO)?.t).toBe('fetch');
    expect(parseWorkerMessage(m({ t: 'fetch.abort', id: 1 }), CO)).toEqual({ t: 'fetch.abort', id: 1 });
  });
  test('refused', () => {
    for (const bad of [
      { ...fetch, url: 'https://evil.example/x' },
      { ...fetch, url: `${CO}.evil.com/x` },
      { ...fetch, url: `${CO}/__vk/sw.js` },
      { ...fetch, method: 'CONNECT' },
      { ...fetch, id: 0 },
      { ...fetch, id: 1.5 },
      { ...fetch, headers: [['Bad Header', 'x']] },
      { ...fetch, headers: [['X', 'a\r\nb']] },
      { ...fetch, body: 'string body' },
      { ...fetch, body: new ArrayBuffer(2) },
      { ...fetch, nav: 'yes' },
      { ...fetch, extra: true },
      { ...fetch, p: 'other' },
    ]) {
      expect(parseWorkerMessage(bad, CO)).toBeNull();
    }
    expect(parseWorkerMessage(m({ t: 'ws.open', id: 1, url: `wss://x/`, protocols: [] }), CO)).toBeNull();
  });
});

describe('page messages', () => {
  const pick = {
    kind: 'element',
    selector: 'main > button',
    role: 'button',
    name: 'Save',
    text: 'Save',
    html: '<button>Save</button>',
    rect: { x: 1, y: 2, w: 30, h: 10 },
    url: `${CO}/settings`,
    viewport: { w: 390, h: 844, dpr: 3 },
  };
  test('valid messages', () => {
    expect(parsePageMessage(m({ t: 'ws.open', id: 2, url: 'wss://abcdefghijklmnop.p.vibeke.dev/?token=x', protocols: ['vite-hmr'] }), CO)?.t).toBe('ws.open');
    expect(parsePageMessage(m({ t: 'ws.send', id: 2, data: 'hi' }), CO)).toEqual({ t: 'ws.send', id: 2, data: 'hi' });
    expect(parsePageMessage(m({ t: 'ws.close', id: 2, code: 1000 }), CO)).toEqual({ t: 'ws.close', id: 2, code: 1000 });
    expect(parsePageMessage(m({ t: 'pick', pick }), CO)).toEqual({ t: 'pick', pick: pick as never });
    expect(parsePageMessage(m({ t: 'cookies.get', path: '/' }), CO)).toEqual({ t: 'cookies.get', path: '/' });
    expect(parsePageMessage(m({ t: 'cookies.set', cookie: 'a=1', path: '/x' }), CO)).toEqual({ t: 'cookies.set', cookie: 'a=1', path: '/x' });
  });
  test('refused', () => {
    for (const bad of [
      { t: 'ws.open', id: 2, url: 'wss://evil.example/', protocols: [] },
      { t: 'ws.open', id: 2, url: 'wss://abcdefghijklmnop.p.vibeke.dev.evil/', protocols: [] },
      { t: 'ws.open', id: 2, url: 'wss://abcdefghijklmnop.p.vibeke.dev/', protocols: ['a b'] },
      { t: 'ws.close', id: 2, code: 1006 },
      { t: 'ws.send', id: 2, data: 5 },
      { t: 'pick', pick: { ...pick, url: 'https://evil.example/' } },
      { t: 'pick', pick: { ...pick, kind: 'page' } },
      { t: 'pick', pick: { ...pick, rect: { x: 1, y: 2, w: -1, h: 3 } } },
      { t: 'pick', pick: { ...pick, extra: 1 } },
      { t: 'pick', pick: { ...pick, covers: Array.from({ length: 11 }, () => ({ selector: 'a' })) } },
      { t: 'cookies.set', cookie: 'a=1', path: 'relative' },
      { t: 'cookies.get' },
      { t: 'fetch', id: 1 },
      { t: 'tunnel.open', preview: 'x' },
      { t: 'agent.prompt', text: 'x' },
    ]) {
      expect(parsePageMessage(m(bad), CO)).toBeNull();
    }
    expect(parsePageMessage(null, CO)).toBeNull();
    expect(parsePageMessage('ws.open', CO)).toBeNull();
  });
});
