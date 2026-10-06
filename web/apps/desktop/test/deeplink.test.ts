import { describe, expect, test } from 'bun:test';
import { deepLinkFromArgv, deepLinkToHash } from '../src/main/deeplink';

const D = 'eyJ2IjoxLCJyZWxheSI6IndzczovL3IiLCJob3N0IjoiaCJ9';

describe('deep links', () => {
  test('pairing links in every accepted form', () => {
    expect(deepLinkToHash(`vibeke://pair?d=${D}`)).toBe(`#/pair?d=${D}`);
    expect(deepLinkToHash(`vibeke:pair?d=${D}`)).toBe(`#/pair?d=${D}`);
    expect(deepLinkToHash(`vibeke:///pair?d=${D}`)).toBe(`#/pair?d=${D}`);
    expect(deepLinkToHash(`https://app.vibeke.dev/#/pair?d=${D}`)).toBe(`#/pair?d=${D}`);
    expect(deepLinkToHash(`vibeke://open#/pair?d=${D}`)).toBe(`#/pair?d=${D}`);
  });
  test('app routes', () => {
    expect(deepLinkToHash('vibeke://inbox')).toBe('#/inbox');
    expect(deepLinkToHash('vibeke://i/abc/01HXYZ')).toBe('#/i/abc/01HXYZ');
    expect(deepLinkToHash('vibeke://h/abc/p/01HXYZ')).toBe('#/h/abc/p/01HXYZ');
    expect(deepLinkToHash('vibeke://')).toBe('#/');
  });
  test('anything else is ignored', () => {
    for (const bad of [
      'vibeke://pair',
      'vibeke://pair?d=short',
      `vibeke://pair?d=${D}<script>`,
      'vibeke://evil',
      'vibeke://i/a',
      'vibeke://i/a/b/c',
      'javascript:alert(1)',
      'file:///etc/passwd',
      'https://example.com/',
      'not a url',
      `https://x/#/pair/extra?d=${D}`,
    ]) {
      expect(deepLinkToHash(bad)).toBeNull();
    }
  });
  test('argv scanning (Windows/Linux second instance)', () => {
    expect(deepLinkFromArgv(['/opt/Vibeke/vibeke', '--hidden', `vibeke://pair?d=${D}`])).toBe(`vibeke://pair?d=${D}`);
    expect(deepLinkFromArgv(['/opt/Vibeke/vibeke'])).toBeNull();
  });
});
