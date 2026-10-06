// Transport selection (spec 16 §9.3, §16.1). A host record's `relay` is either a relay WebSocket
// base (`wss://relay.example.com`) or, for a gateway on the same machine, `local:<socket path>`
// (printed by `vibeke gateway pair --local`). Core builds `<relay>/v1/connect?host=<id>` for
// both; shells that can reach Unix sockets (Electron) split it back up with `parseConnectUrl`.

export const LOCAL_PREFIX = 'local:';

export type Transport = 'relay' | 'local';

export const transportOf = (relay: string): Transport => (relay.startsWith(LOCAL_PREFIX) ? 'local' : 'relay');

/** The socket path of a `local:` relay value, or null. */
export function localSocketPath(relay: string): string | null {
  if (!relay.startsWith(LOCAL_PREFIX)) return null;
  const p = relay.slice(LOCAL_PREFIX.length);
  return p.startsWith('/') && !p.includes('\0') ? p : null;
}

export type ConnectTarget =
  | { kind: 'relay'; url: string }
  /** WebSocket over a Unix socket: connect to `socketPath`, request `path`. */
  | { kind: 'local'; socketPath: string; path: string };

/**
 * Split what core passes to `Platform.connect` into a transport. Throws on anything else:
 * only `ws:`/`wss:` relays and absolute `local:` socket paths are accepted.
 */
export function parseConnectUrl(url: string): ConnectTarget {
  if (url.startsWith(LOCAL_PREFIX)) {
    const rest = url.slice(LOCAL_PREFIX.length);
    // The socket path may contain anything (spaces: "Application Support"), so cut at the last
    // `/v1/` that core appended rather than parsing it as a URL.
    const i = rest.lastIndexOf('/v1/');
    const socketPath = i >= 0 ? rest.slice(0, i) : rest;
    const path = i >= 0 ? rest.slice(i) : '/';
    if (!socketPath.startsWith('/') || socketPath.includes('\0') || socketPath.length < 2) throw new Error('transport: bad local socket path');
    if (!/^\/v1\/[A-Za-z0-9/_.-]*(\?[A-Za-z0-9=&%_.-]*)?$/.test(path) && path !== '/') throw new Error('transport: bad local request path');
    return { kind: 'local', socketPath, path };
  }
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    throw new Error('transport: invalid relay URL');
  }
  if (u.protocol !== 'wss:' && u.protocol !== 'ws:') throw new Error(`transport: unsupported scheme ${u.protocol}`);
  if (u.username || u.password || u.hash) throw new Error('transport: relay URL must not carry credentials or a fragment');
  return { kind: 'relay', url: u.toString() };
}
