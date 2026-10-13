import { describe, expect, test } from 'bun:test';
import { b64 } from '@vibeke/core';
import { deepLinkAction, deepLinkFromArgv, deepLinkToHash } from '../src/main/deeplink';

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
    expect(deepLinkToHash('vibeke://w/abc/01HWS')).toBe('#/w/abc/01HWS');
    expect(deepLinkToHash('vibeke://w/abc/01HWS/t/01HXYZ')).toBe('#/w/abc/01HWS/t/01HXYZ');
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
      'vibeke://w/a',
      'vibeke://w/a/b/x/c',
      'vibeke://w/a/b/t',
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

  test('continue links open the continue screen (spec 20 §7.3)', () => {
    expect(deepLinkToHash(`vibeke://continue?d=${D}`)).toBe(`#/continue?d=${D}`);
    expect(deepLinkToHash(`https://app.vibeke.dev/#/continue?d=${D}`)).toBe(`#/continue?d=${D}`);
    expect(deepLinkAction(`vibeke://continue?d=${D}`)).toEqual({ kind: 'route', hash: `#/continue?d=${D}` });
  });
  test('review links and GitHub-gated pair links go to the system browser', () => {
    expect(deepLinkAction(`https://app.example/#/review?d=${D}`)).toEqual({ kind: 'external', url: `https://app.example/#/review?d=${D}` });
    expect(deepLinkAction(`vibeke://review?d=${D}`)).toEqual({ kind: 'external', url: `https://app.vibeke.dev/#/review?d=${D}` });
    const link = {
      v: 1,
      relay: 'wss://relay.example',
      host: 'hostid',
      hk: b64.encode(new Uint8Array(32).fill(1)),
      pid: 'pid-1',
      psk: b64.encode(new Uint8Array(32).fill(2)),
      exp: 1_900_000_000,
      name: 'devbox',
    };
    const enc = (o: unknown) => b64.encode(new TextEncoder().encode(JSON.stringify(o)));
    const gh = enc({ ...link, share: { kind: 'share', scope: 'view', until: 5, access: 'github', uses: 3 } });
    expect(deepLinkAction(`vibeke://pair?d=${gh}`)).toEqual({ kind: 'external', url: `https://app.vibeke.dev/#/pair?d=${gh}` });
    const plain = enc({ ...link, share: { kind: 'share', scope: 'view', until: 5 } });
    expect(deepLinkAction(`vibeke://pair?d=${plain}`)).toEqual({ kind: 'route', hash: `#/pair?d=${plain}` });
    expect(deepLinkAction('vibeke://evil')).toBeNull();
  });
});
