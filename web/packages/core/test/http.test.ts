import { describe, expect, test } from 'bun:test';
import { forwardRequestHeaders, forwardResponseHeaders, HttpParseError, ResponseParser, rewriteLocation, writeRequest, type ResponseEvent } from '../src/http';

const te = new TextEncoder();
const td = new TextDecoder();
const bytes = (s: string) => te.encode(s);

/** Feed `raw` one byte at a time and in one piece; both must give the same body. */
function parseAll(raw: string, method = 'GET', split = 1): { status: number; headers: [string, string][]; body: string; ended: boolean } {
  const p = new ResponseParser(method);
  const ev: ResponseEvent[] = [];
  const b = bytes(raw);
  for (let i = 0; i < b.length; i += split) ev.push(...p.push(b.subarray(i, i + split)));
  if (!p.done) ev.push(...p.finish());
  const head = ev.find((e) => e.type === 'head') as Extract<ResponseEvent, { type: 'head' }> | undefined;
  const body = ev.filter((e) => e.type === 'data').map((e) => td.decode((e as { data: Uint8Array }).data)).join('');
  return { status: head?.head.status ?? 0, headers: head?.head.headers ?? [], body, ended: ev.some((e) => e.type === 'end') };
}

describe('request writer', () => {
  test('head and body', () => {
    const out = td.decode(writeRequest({ method: 'POST', target: '/api?x=1', headers: [['Host', 'localhost:5173']] }, bytes('hi')));
    expect(out).toBe('POST /api?x=1 HTTP/1.1\r\nHost: localhost:5173\r\nContent-Length: 2\r\n\r\nhi');
    expect(td.decode(writeRequest({ method: 'GET', target: '/', headers: [] }))).toBe('GET / HTTP/1.1\r\n\r\n');
    expect(td.decode(writeRequest({ method: 'POST', target: '/', headers: [] }))).toContain('Content-Length: 0');
  });

  test('refuses header injection', () => {
    expect(() => writeRequest({ method: 'GET', target: '/', headers: [['X', 'a\r\nEvil: 1']] })).toThrow();
    expect(() => writeRequest({ method: 'GET', target: '/ HTTP/1.1\r\n', headers: [] })).toThrow();
    expect(() => writeRequest({ method: 'G T', target: '/', headers: [] })).toThrow();
    expect(() => writeRequest({ method: 'GET', target: 'http://evil/', headers: [] })).toThrow();
  });

  test('forwarded headers: host, origin, referer, cookie, encoding', () => {
    const co = 'https://abcdefghijklmnop.p.vibeke.dev';
    const h = forwardRequestHeaders(
      [
        ['Host', 'abcdefghijklmnop.p.vibeke.dev'],
        ['Origin', co],
        ['Referer', `${co}/settings?a=1`],
        ['Cookie', 'browser=1'],
        ['Accept-Encoding', 'gzip, br'],
        ['Connection', 'keep-alive'],
        ['Accept', 'text/html'],
      ],
      { target: 'localhost:5173', contentOrigin: co, cookie: 'sid=abc' },
    );
    expect(h).toEqual([
      ['Host', 'localhost:5173'],
      ['Origin', 'http://localhost:5173'],
      ['Referer', 'http://localhost:5173/settings?a=1'],
      ['Accept', 'text/html'],
      ['Accept-Encoding', 'identity'],
      ['Cookie', 'sid=abc'],
      ['Connection', 'close'],
    ]);
    const up = forwardRequestHeaders([['Sec-WebSocket-Key', 'x']], { target: 'localhost:1', contentOrigin: co, cookie: null, upgrade: true });
    expect(up.slice(-2)).toEqual([
      ['Connection', 'Upgrade'],
      ['Upgrade', 'websocket'],
    ]);
  });

  test('response headers: no Set-Cookie, Location moved, frame-ancestors dropped', () => {
    const co = 'https://abcdefghijklmnop.p.vibeke.dev';
    const h = forwardResponseHeaders(
      [
        ['Set-Cookie', 'a=1'],
        ['Location', 'http://localhost:5173/login?next=/'],
        ['Content-Security-Policy', "default-src 'self'; frame-ancestors 'none'"],
        ['X-Frame-Options', 'DENY'],
        ['Transfer-Encoding', 'chunked'],
        ['Content-Type', 'text/html'],
        ['Content-Encoding', 'gzip'],
      ],
      { target: 'localhost:5173', contentOrigin: co, decoded: true },
    );
    expect(h).toEqual([
      ['Location', `${co}/login?next=/`],
      ['Content-Security-Policy', "default-src 'self'"],
      ['Content-Type', 'text/html'],
    ]);
    expect(rewriteLocation('http://127.0.0.1:5173', 'localhost:5173', co)).toBe(`${co}/`);
    expect(rewriteLocation('https://github.com/login', 'localhost:5173', co)).toBe('https://github.com/login');
    expect(rewriteLocation('http://localhost:51730/x', 'localhost:5173', co)).toBe('http://localhost:51730/x');
  });
});

describe('response parser', () => {
  test('content-length, any split', () => {
    const raw = 'HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello';
    for (const split of [1, 3, 1000]) expect(parseAll(raw, 'GET', split)).toEqual({ status: 200, headers: [['Content-Type', 'text/plain'], ['Content-Length', '5']], body: 'hello', ended: true });
  });

  test('chunked with extensions and trailers', () => {
    const raw = 'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;x=y\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\n';
    for (const split of [1, 7, 1000]) {
      const r = parseAll(raw, 'GET', split);
      expect(r.body).toBe('hello world');
      expect(r.ended).toBe(true);
    }
  });

  test('close-delimited body ends with the stream', () => {
    const r = parseAll('HTTP/1.0 200 OK\r\n\r\nall of it');
    expect(r).toMatchObject({ status: 200, body: 'all of it', ended: true });
  });

  test('no body for HEAD, 204 and 304; 100 Continue is skipped', () => {
    expect(parseAll('HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n', 'HEAD')).toMatchObject({ body: '', ended: true });
    expect(parseAll('HTTP/1.1 204 No Content\r\n\r\n')).toMatchObject({ status: 204, ended: true });
    expect(parseAll('HTTP/1.1 304 Not Modified\r\nContent-Length: 3\r\n\r\n')).toMatchObject({ status: 304, body: '' });
    expect(parseAll('HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok')).toMatchObject({ status: 201, body: 'ok' });
  });

  test('repeated Set-Cookie headers stay separate', () => {
    const r = parseAll('HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Path=/\r\nSet-Cookie: b=2; Expires=Wed, 21 Oct 2037 07:28:00 GMT\r\nContent-Length: 0\r\n\r\n');
    expect(r.headers.filter(([k]) => k === 'Set-Cookie').map(([, v]) => v)).toEqual(['a=1; Path=/', 'b=2; Expires=Wed, 21 Oct 2037 07:28:00 GMT']);
  });

  test('upgrade hands over the rest of the bytes', () => {
    const p = new ResponseParser();
    const ev = p.push(bytes('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n\x81\x02hi'));
    expect(ev).toHaveLength(1);
    const u = ev[0] as Extract<ResponseEvent, { type: 'upgrade' }>;
    expect(u.type).toBe('upgrade');
    expect(u.head.status).toBe(101);
    expect(Array.from(u.rest)).toEqual([0x81, 2, 0x68, 0x69]);
    expect(p.push(bytes('more'))).toEqual([{ type: 'data', data: bytes('more') }]);
  });

  test('errors', () => {
    expect(() => new ResponseParser().push(bytes('SSH-2.0\r\n\r\n'))).toThrow(HttpParseError);
    expect(() => new ResponseParser().push(bytes('HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n'))).toThrow('conflicting');
    expect(() => new ResponseParser().push(bytes('HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n'))).toThrow('bad chunk size');
    const p = new ResponseParser();
    p.push(bytes('HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc'));
    expect(() => p.finish()).toThrow('mid-body');
    expect(() => new ResponseParser().finish()).toThrow('before a response');
    expect(() => new ResponseParser().push(new Uint8Array(70 * 1024).fill(65))).toThrow('too large');
  });
});
