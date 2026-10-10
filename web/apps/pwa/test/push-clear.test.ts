import { describe, expect, test } from 'bun:test';
import { isAppleWebKit } from '../src/detect';
import { parseClear, windowShowsTarget } from '../src/push-payload';

const SCOPE = 'https://app.example.com/';
const win = (hash: string, over: Partial<{ visible: boolean; focused: boolean; origin: string }> = {}) => ({
  url: `${over.origin ?? 'https://app.example.com'}/${hash}`,
  visible: over.visible ?? true,
  focused: over.focused ?? true,
});

describe('clear payload', () => {
  test('uses the tag, else the host tag', () => {
    expect(parseClear({ kind: 'clear', tag: 'vibeke:h1', host: 'h1' })).toEqual({ tag: 'vibeke:h1' });
    expect(parseClear({ kind: 'clear', host: 'h1' })).toEqual({ tag: 'vibeke:h1' });
  });
  test('ignores other payloads and tagless clears', () => {
    expect(parseClear({ title: 'x', tag: 'vibeke:h1' })).toBeNull();
    expect(parseClear({ kind: 'clear' })).toBeNull();
    expect(parseClear('clear')).toBeNull();
    expect(parseClear(null)).toBeNull();
  });
});

describe('suppressing a notification the app already shows', () => {
  test('a visible focused inbox or the target route suppresses it', () => {
    expect(windowShowsTarget([win('#/inbox')], SCOPE, '#/i/h1/a')).toBe(true);
    expect(windowShowsTarget([win('#/i/h1/a')], SCOPE, '#/i/h1/a')).toBe(true);
  });
  test('other routes, hidden, unfocused or foreign windows do not', () => {
    expect(windowShowsTarget([win('#/settings')], SCOPE, '#/i/h1/a')).toBe(false);
    expect(windowShowsTarget([win('#/inbox', { visible: false })], SCOPE, '#/i/h1/a')).toBe(false);
    expect(windowShowsTarget([win('#/inbox', { focused: false })], SCOPE, '#/i/h1/a')).toBe(false);
    expect(windowShowsTarget([win('#/inbox', { origin: 'https://other.example.com' })], SCOPE, '#/i/h1/a')).toBe(false);
  });
  test('never suppresses when no window is open', () => {
    expect(windowShowsTarget([], SCOPE, '#/inbox')).toBe(false);
  });
});

describe('Apple WebKit detection', () => {
  const iphone = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1';
  const iosChrome = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/126.0 Mobile/15E148 Safari/604.1';
  const macSafari = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15';
  const macChrome = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36';
  const android = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36';
  const firefox = 'Mozilla/5.0 (X11; Linux x86_64; rv:127.0) Gecko/20100101 Firefox/127.0';
  test('Safari and every iOS browser are WebKit', () => {
    expect(isAppleWebKit(iphone, 'iPhone', 5)).toBe(true);
    expect(isAppleWebKit(iosChrome, 'iPhone', 5)).toBe(true);
    expect(isAppleWebKit(macSafari, 'MacIntel', 0)).toBe(true);
  });
  test('Chrome, Edge, Android and Firefox are not', () => {
    expect(isAppleWebKit(macChrome, 'MacIntel', 0)).toBe(false);
    expect(isAppleWebKit(android, 'Linux armv8l', 5)).toBe(false);
    expect(isAppleWebKit(firefox, 'Linux x86_64', 0)).toBe(false);
  });
});
