import { describe, expect, test } from 'bun:test';
import { parseConnectUrl, relayConnectUrl, transportOf } from '@vibeke/core';
import { NodeSocket, wsArgs } from '../src/main/transport';

describe('transport selection', () => {
  test('relay URLs go over WebSocket as-is', () => {
    const t = parseConnectUrl(relayConnectUrl('wss://relay.example.com/', 'abc'));
    expect(t).toEqual({ kind: 'relay', url: 'wss://relay.example.com/v1/connect?host=abc' });
    expect(wsArgs(t).url).toBe('wss://relay.example.com/v1/connect?host=abc');
    expect(transportOf('wss://relay.example.com')).toBe('relay');
  });

  test('local: links keep paths with spaces and colons intact', () => {
    const sock = '/Users/me/Library/Application Support/vibeke/gateway/gateway.sock';
    const t = parseConnectUrl(relayConnectUrl(`local:${sock}`, 'abc'));
    expect(t).toEqual({ kind: 'local', socketPath: sock, path: '/v1/connect?host=abc' });
    const a = wsArgs(t);
    expect(a.url).toBe('ws://localhost/v1/connect?host=abc');
    expect(typeof (a.options as { createConnection?: unknown }).createConnection).toBe('function');
    expect(transportOf(`local:${sock}`)).toBe('local');
  });

  test('rejects other schemes and malformed local paths', () => {
    for (const bad of ['http://x/v1/connect', 'file:///tmp/x', 'local:relative/x.sock/v1/connect', 'local:/v1/connect', 'wss://u:p@relay/x', 'nonsense']) {
      expect(() => parseConnectUrl(bad)).toThrow();
    }
  });

  // The Unix-socket round trip itself runs under Electron's Node in e2e/local-gateway.e2e.ts:
  // Bun replaces `ws` with its own client, which has no `createConnection`.

  test('a missing socket reports a close, never throws', async () => {
    const s = new NodeSocket('local:/nonexistent/dir/gateway.sock/v1/connect?host=h');
    const code = await new Promise<number>((r) => (s.onclose = (c) => r(c)));
    expect(code).toBe(1006);
    const bad = new NodeSocket('ftp://nope');
    expect(await new Promise<number>((r) => (bad.onclose = (c) => r(c)))).toBe(1006);
  });
});
