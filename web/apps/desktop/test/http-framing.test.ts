import { describe, expect, test } from 'bun:test';
import { ChunkedTracker, keepAlive, parseResponseHead, requestFraming, responseFraming } from '../src/main/http-framing';

describe('chunked bodies', () => {
  test('finds the end across any split', () => {
    const body = Buffer.from('5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nTrailer: x\r\n\r\nNEXT');
    for (let cut = 1; cut < body.length; cut++) {
      const t = new ChunkedTracker();
      const a = t.feed(body.subarray(0, cut));
      if (a.done) {
        expect(a.used).toBe(body.length - 4);
        continue;
      }
      expect(a.used).toBe(cut);
      const b = t.feed(body.subarray(cut));
      expect(b.done).toBe(true);
      expect(cut + b.used).toBe(body.length - 4);
    }
  });
  test('bad sizes are refused', () => {
    expect(() => new ChunkedTracker().feed(Buffer.from('zz\r\n'))).toThrow();
  });
});

describe('framing', () => {
  test('requests', () => {
    expect(requestFraming([])).toEqual({ kind: 'none' });
    expect(requestFraming([['Content-Length', '12']])).toEqual({ kind: 'length', n: 12 });
    expect(requestFraming([['Transfer-Encoding', 'chunked']])).toEqual({ kind: 'chunked' });
    expect(requestFraming([['Content-Length', 'x']])).toBeNull();
    expect(requestFraming([['Transfer-Encoding', 'gzip']])).toBeNull();
  });
  test('responses', () => {
    const r = (status: number, headers: [string, string][] = []) => ({ version: '1.1' as const, status, headers });
    expect(responseFraming('HEAD', r(200, [['Content-Length', '10']]))).toEqual({ kind: 'none' });
    expect(responseFraming('GET', r(304))).toEqual({ kind: 'none' });
    expect(responseFraming('GET', r(204))).toEqual({ kind: 'none' });
    expect(responseFraming('GET', r(200))).toEqual({ kind: 'close' });
    expect(responseFraming('GET', r(200, [['Content-Length', '3']]))).toEqual({ kind: 'length', n: 3 });
    expect(responseFraming('GET', r(200, [['Transfer-Encoding', 'gzip, chunked']]))).toEqual({ kind: 'chunked' });
  });
  test('keep-alive rules', () => {
    expect(keepAlive('1.1', [])).toBe(true);
    expect(keepAlive('1.1', [['Connection', 'close']])).toBe(false);
    expect(keepAlive('1.0', [])).toBe(false);
    expect(keepAlive('1.0', [['Connection', 'keep-alive']])).toBe(true);
  });
  test('response heads', () => {
    expect(parseResponseHead('HTTP/1.1 200 OK\r\nContent-Length: 1')).toEqual({ version: '1.1', status: 200, headers: [['Content-Length', '1']] });
    expect(parseResponseHead('tunnel:GET / HTTP/1.1')).toBeNull();
  });
});
