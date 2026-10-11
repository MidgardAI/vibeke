import { afterEach, describe, expect, test } from 'bun:test';
import { connect, createServer, type Server, type Socket } from 'node:net';
import type { Duplex } from 'node:stream';
import { PreviewProxy, isLoopbackName, isOurProxyChallenge, loopbackIp, originRequest, parseAuthority, parseHead, resolveDestination, type PreviewRoute } from '../src/main/preview-proxy';

/** A TCP server that answers every connection with a fixed prefix and then echoes. */
async function echoServer(tag: string): Promise<{ server: Server; port: number; received: string[] }> {
  const received: string[] = [];
  const server = createServer((s) => {
    s.write(`${tag}:`);
    s.on('data', (d) => {
      received.push(d.toString('latin1'));
      s.write(d);
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const a = server.address();
  return { server, port: typeof a === 'object' && a ? a.port : 0, received };
}

function dial(port: number): Promise<Socket> {
  return new Promise((resolve, reject) => {
    const s = connect({ host: '127.0.0.1', port }, () => resolve(s));
    s.once('error', reject);
  });
}

/** Reads from a socket until `pred(text)` holds (or the socket ends). */
function readUntil(s: Socket, pred: (text: string) => boolean): Promise<string> {
  return new Promise((resolve) => {
    let text = '';
    const done = () => {
      s.off('data', on);
      resolve(text);
    };
    const on = (d: Buffer) => {
      text += d.toString('latin1');
      if (pred(text)) done();
    };
    s.on('data', on);
    s.once('end', done);
    s.once('close', done);
  });
}

const basic = (user: string, pass: string) => `Basic ${Buffer.from(`${user}:${pass}`).toString('base64')}`;

const cleanups: (() => void)[] = [];
afterEach(() => {
  for (const f of cleanups.splice(0)) f();
});

async function setup() {
  const preview = await echoServer('tunnel');
  const outside = await echoServer('direct');
  const tunnels: { route: PreviewRoute; host: string; port: number }[] = [];
  const directs: { address: string; port: number }[] = [];
  const proxy = new PreviewProxy({
    // The fake tunnel: a TCP connection to the "preview machine" echo server.
    tunnel: async (route, host, port) => {
      tunnels.push({ route, host, port });
      return (await dial(preview.port)) as Duplex;
    },
    direct: async (address, port) => {
      directs.push({ address, port });
      return (await dial(outside.port)) as Duplex;
    },
    lookup: async (host) => {
      if (host === 'devbox.test') return [{ address: '127.0.0.1', family: 4 }];
      if (host === 'mixed.test') return [{ address: '127.0.0.1', family: 4 }, { address: '203.0.113.7', family: 4 }];
      return [{ address: '198.51.100.20', family: 4 }];
    },
  });
  const port = await proxy.start();
  const cred = proxy.addView('pvaaaaaaaaaaaaaaaaaaaaaaaa', { hostId: 'HOST1', machine: 'devbox' });
  cleanups.push(() => {
    proxy.close();
    preview.server.close();
    outside.server.close();
  });
  return { proxy, port, cred, tunnels, directs, preview, outside };
}

describe('preview proxy', () => {
  test('requests without valid credentials get 407', async () => {
    const { port, cred } = await setup();
    for (const auth of [null, basic(cred.username, 'wrong'), basic('pvbbbbbbbbbbbbbbbbbbbbbbbb', cred.password), 'Bearer x']) {
      const s = await dial(port);
      s.write(`CONNECT localhost:5173 HTTP/1.1\r\nHost: localhost:5173\r\n${auth ? `Proxy-Authorization: ${auth}\r\n` : ''}\r\n`);
      const text = await readUntil(s, (t) => t.includes('\r\n\r\n'));
      expect(text.startsWith('HTTP/1.1 407')).toBe(true);
      expect(text).toContain('Proxy-Authenticate: Basic realm="vibeke-preview"');
      s.destroy();
    }
  });

  test('a removed view loses its credential', async () => {
    const { proxy, cred } = await setup();
    expect(proxy.authenticate(basic(cred.username, cred.password))).toEqual({ hostId: 'HOST1', machine: 'devbox' });
    proxy.removeView(cred.username);
    expect(proxy.authenticate(basic(cred.username, cred.password))).toBeNull();
  });

  test('CONNECT to a loopback destination goes through the tunnel', async () => {
    const { port, cred, tunnels, directs } = await setup();
    const s = await dial(port);
    s.write(`CONNECT localhost:5173 HTTP/1.1\r\nHost: localhost:5173\r\nProxy-Authorization: ${basic(cred.username, cred.password)}\r\n\r\n`);
    const established = await readUntil(s, (t) => t.includes('tunnel:'));
    expect(established).toStartWith('HTTP/1.1 200 Connection Established\r\n\r\n');
    s.write('ping');
    const echoed = await readUntil(s, (t) => t.includes('ping'));
    expect(echoed).toContain('ping');
    expect(tunnels).toEqual([{ route: { hostId: 'HOST1', machine: 'devbox' }, host: 'localhost', port: 5173 }]);
    expect(directs).toEqual([]);
    s.destroy();
  });

  test('names that resolve only to loopback go to the preview machine', async () => {
    const { port, cred, tunnels, directs } = await setup();
    const s = await dial(port);
    s.write(`CONNECT devbox.test:8443 HTTP/1.1\r\nProxy-Authorization: ${basic(cred.username, cred.password)}\r\n\r\n`);
    await readUntil(s, (t) => t.includes('tunnel:'));
    expect(tunnels[0]).toMatchObject({ host: '127.0.0.1', port: 8443 });
    expect(directs).toEqual([]);
    s.destroy();
  });

  test('other destinations connect directly, never to a loopback address', async () => {
    const { port, cred, tunnels, directs } = await setup();
    const s = await dial(port);
    s.write(`CONNECT mixed.test:443 HTTP/1.1\r\nProxy-Authorization: ${basic(cred.username, cred.password)}\r\n\r\n`);
    await readUntil(s, (t) => t.includes('direct:'));
    expect(directs).toEqual([{ address: '203.0.113.7', port: 443 }]);
    expect(tunnels).toEqual([]);
    s.destroy();
  });

  test('absolute-form requests are rewritten to origin form', async () => {
    const { port, cred, preview, tunnels } = await setup();
    const s = await dial(port);
    s.write(
      `GET http://localhost:3000/app/page?x=1 HTTP/1.1\r\nHost: localhost:3000\r\nProxy-Authorization: ${basic(cred.username, cred.password)}\r\n` +
        'Proxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: text/html\r\n\r\n',
    );
    await readUntil(s, (t) => t.includes('Accept: text/html'));
    const sent = preview.received.join('');
    expect(sent).toStartWith('GET /app/page?x=1 HTTP/1.1\r\n');
    expect(sent).toContain('Host: localhost:3000\r\n');
    expect(sent).toContain('Accept: text/html\r\n');
    expect(sent).toContain('Connection: close\r\n');
    expect(sent.toLowerCase()).not.toContain('proxy-authorization');
    expect(sent.toLowerCase()).not.toContain('proxy-connection');
    expect(sent.toLowerCase()).not.toContain('keep-alive');
    expect(tunnels[0]).toMatchObject({ host: 'localhost', port: 3000 });
    s.destroy();
  });

  test('a tunnel failure answers 502', async () => {
    const proxy = new PreviewProxy({
      tunnel: async () => {
        throw new Error('refused');
      },
    });
    const port = await proxy.start();
    cleanups.push(() => proxy.close());
    const cred = proxy.addView('pvcccccccccccccccccccccccc', { hostId: 'H', machine: '' });
    const s = await dial(port);
    s.write(`CONNECT localhost:1 HTTP/1.1\r\nProxy-Authorization: ${basic(cred.username, cred.password)}\r\n\r\n`);
    const text = await readUntil(s, (t) => t.includes('refused'));
    expect(text).toStartWith('HTTP/1.1 502');
    s.destroy();
  });
});

describe('destinations and heads', () => {
  test('loopback detection', () => {
    for (const h of ['localhost', 'LOCALHOST.', 'app.localhost', '127.0.0.1', '127.1.2.3', '[::1]', '::1', '0:0:0:0:0:0:0:1', '::ffff:127.0.0.1']) expect(isLoopbackName(h)).toBe(true);
    for (const h of ['example.com', 'localhost.example.com', '10.0.0.1', '::2', '[2001:db8::1]', '::ffff:10.0.0.1']) expect(isLoopbackName(h)).toBe(false);
    expect(loopbackIp('0.0.0.0')).toBe('127.0.0.1');
    expect(loopbackIp('::')).toBe('::1');
    expect(loopbackIp('::ffff:127.0.0.1')).toBe('127.0.0.1');
  });

  test('resolveDestination', async () => {
    const lookup = async (h: string) => (h === 'only.test' ? [{ address: '::1', family: 6 }] : [{ address: '192.0.2.1', family: 4 }]);
    expect(await resolveDestination('localhost', 80, lookup)).toEqual({ kind: 'tunnel', host: 'localhost', port: 80 });
    expect(await resolveDestination('[::1]', 80, lookup)).toEqual({ kind: 'tunnel', host: '::1', port: 80 });
    expect(await resolveDestination('only.test', 80, lookup)).toEqual({ kind: 'tunnel', host: '::1', port: 80 });
    expect(await resolveDestination('cdn.test', 443, lookup)).toEqual({ kind: 'direct', address: '192.0.2.1', port: 443 });
    expect(await resolveDestination('10.1.2.3', 22, lookup)).toEqual({ kind: 'direct', address: '10.1.2.3', port: 22 });
    await expect(resolveDestination('bad host', 80, lookup)).rejects.toThrow();
    await expect(resolveDestination('localhost', 0, lookup)).rejects.toThrow();
  });

  test('parseHead, parseAuthority, originRequest', () => {
    expect(parseHead('GET / HTTP/1.1\r\nHost: x')).toEqual({ method: 'GET', target: '/', version: 'HTTP/1.1', headers: [['Host', 'x']] });
    expect(parseHead('garbage')).toBeNull();
    expect(parseHead('GET / HTTP/1.1\r\nno colon')).toBeNull();
    expect(parseAuthority('[::1]:443')).toEqual({ host: '::1', port: 443 });
    expect(parseAuthority('localhost:70000')).toBeNull();
    expect(parseAuthority('localhost')).toBeNull();
    expect(originRequest(parseHead('GET https://x/ HTTP/1.1')!)).toBeNull();
    expect(originRequest(parseHead('GET http://u:p@x/ HTTP/1.1')!)).toBeNull();
    const ws = originRequest(parseHead('GET http://localhost:5173/hmr HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket')!)!;
    expect(ws.head).toContain('Upgrade: websocket\r\nConnection: Upgrade\r\n');
    expect(ws.head).toContain('Host: localhost:5173\r\n');
    expect(ws.head).not.toContain('Connection: close');
  });

  test('login challenges are ours only for the proxy port on 127.0.0.1', () => {
    expect(isOurProxyChallenge({ isProxy: true, host: '127.0.0.1', port: 4000 }, 4000)).toBe(true);
    expect(isOurProxyChallenge({ isProxy: false, host: '127.0.0.1', port: 4000 }, 4000)).toBe(false);
    expect(isOurProxyChallenge({ isProxy: true, host: '127.0.0.1', port: 4001 }, 4000)).toBe(false);
    expect(isOurProxyChallenge({ isProxy: true, host: 'proxy.corp', port: 4000 }, 4000)).toBe(false);
    expect(isOurProxyChallenge({ isProxy: true, host: '127.0.0.1', port: 0 }, 0)).toBe(false);
  });
});
