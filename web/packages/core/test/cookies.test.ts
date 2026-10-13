import { describe, expect, test } from 'bun:test';
import { CookieJar, defaultPath, parseSetCookie, pathMatches } from '../src/cookies';

const NOW = Date.parse('2030-01-01T00:00:00Z');

describe('Set-Cookie parsing', () => {
  test('attributes', () => {
    const c = parseSetCookie('sid=abc; Path=/app; HttpOnly; Secure; SameSite=Lax; Expires=Wed, 21 Oct 2037 07:28:00 GMT', NOW)!;
    expect(c).toMatchObject({ name: 'sid', value: 'abc', path: '/app', httpOnly: true, secure: true, sameSite: 'lax', expires: Date.parse('Wed, 21 Oct 2037 07:28:00 GMT') });
  });
  test('Max-Age wins over Expires; default path from the request', () => {
    const c = parseSetCookie('a=1; Expires=Wed, 21 Oct 2037 07:28:00 GMT; Max-Age=60', NOW, '/docs/page')!;
    expect(c.expires).toBe(NOW + 60_000);
    expect(c.path).toBe('/docs');
    expect(parseSetCookie('a=1; Max-Age=0', NOW)!.expires).toBe(0);
  });
  test('ignored headers', () => {
    expect(parseSetCookie('noequals', NOW)).toBeNull();
    expect(parseSetCookie('=x', NOW)).toBeNull();
    expect(parseSetCookie('a b=1', NOW)).toBeNull();
    expect(parseSetCookie('a=1\n; Path=/', NOW)).toBeNull();
  });
  test('path helpers', () => {
    expect(defaultPath('/')).toBe('/');
    expect(defaultPath('/a')).toBe('/');
    expect(defaultPath('/a/b?x=/y')).toBe('/a');
    expect(pathMatches('/app/x', '/app')).toBe(true);
    expect(pathMatches('/app', '/app')).toBe(true);
    expect(pathMatches('/apple', '/app')).toBe(false);
    expect(pathMatches('/app/x', '/app/')).toBe(true);
  });
});

describe('cookie jar', () => {
  test('request header, ordering and expiry', () => {
    const jar = new CookieJar();
    jar.setFromHeader('a=1; Path=/', NOW);
    jar.setFromHeader('b=2; Path=/app', NOW + 1);
    jar.setFromHeader('c=3; Path=/; Max-Age=10', NOW + 2);
    expect(jar.header('/app/x', NOW + 3)).toBe('b=2; a=1; c=3');
    expect(jar.header('/other', NOW + 3)).toBe('a=1; c=3');
    expect(jar.header('/other', NOW + 20_000)).toBe('a=1');
    jar.setFromHeader('a=gone; Path=/; Max-Age=0', NOW);
    expect(jar.header('/', NOW)).toBe('c=3');
  });

  test('HttpOnly cookies never reach document.cookie and cannot be replaced from it', () => {
    const jar = new CookieJar();
    jar.setFromHeader('session=secret; Path=/; HttpOnly', NOW);
    jar.setFromHeader('theme=dark; Path=/', NOW);
    expect(jar.visible('/', NOW)).toBe('theme=dark');
    expect(jar.setFromDocument('session=evil; path=/', NOW)).toBe(false);
    expect(jar.setFromDocument('x=1; HttpOnly', NOW)).toBe(false);
    expect(jar.setFromDocument('lang=nb', NOW, '/')).toBe(true);
    expect(jar.header('/', NOW)).toBe('session=secret; theme=dark; lang=nb');
    expect(jar.visible('/', NOW)).toBe('theme=dark; lang=nb');
  });

  test('a replaced cookie keeps its creation order; round trip through storage', () => {
    const jar = new CookieJar();
    jar.setFromHeader('a=1', NOW);
    jar.setFromHeader('b=1', NOW + 1);
    jar.setFromHeader('a=2', NOW + 2);
    expect(jar.header('/', NOW + 3)).toBe('a=2; b=1');
    const again = new CookieJar(JSON.parse(JSON.stringify(jar.toJSON(NOW + 3))));
    expect(again.header('/', NOW + 3)).toBe('a=2; b=1');
    expect(new CookieJar({ v: 1, cookies: [{ bogus: true } as never] }).all(NOW)).toEqual([]);
  });
});
