// A render stream within the paired host's existing encrypted RPC connection.
import * as b64 from './b64';
import type { RpcClient } from './rpc';

export interface TuiStream {
  readonly id: string;
  readonly clientId: string;
  readonly features: string[];
  /** Start delivery after the WASM client has accepted the attach result. */
  start(): void;
  send(data: Uint8Array): Promise<void>;
  close(): void;
}
export interface TuiCallbacks {
  frame(data: Uint8Array): void;
  closed(reason: string): void;
}

export async function openTuiStream(rpc: RpcClient, protocol: number, cb: TuiCallbacks, signal?: AbortSignal): Promise<TuiStream> {
  let id: string | null = null;
  let started = false;
  let closed = false;
  let queued = 0;
  let pending: Array<{ stream: string; data?: Uint8Array; reason?: string }> = [];
  const cleanup = () => { offFrame(); offClose(); signal?.removeEventListener('abort', close); pending = []; queued = 0; };
  const close = () => {
    if (closed) return;
    closed = true;
    cleanup();
    if (id && !rpc.closed) void rpc.request('tui.detach', { stream: id }).catch(() => {});
  };
  const fail = (reason: string) => { close(); cb.closed(reason); };
  const receive = (event: { stream: string; data?: Uint8Array; reason?: string }) => {
    if (closed || (id !== null && event.stream !== id)) return;
    if (!started) {
      queued += event.data?.byteLength ?? 0;
      if (queued > 8 * 1024 * 1024 || pending.length >= 256) return fail('TUI attach buffer is full');
      pending.push(event);
      return;
    }
    if (event.reason !== undefined) fail(event.reason);
    else if (event.data) {
      try { cb.frame(event.data); } catch (e) { fail(String(e)); }
    }
  };
  const offFrame = rpc.on('tui.frame', (p) => {
    const v = p as { stream?: unknown; data?: unknown };
    if (closed || typeof v.stream !== 'string' || typeof v.data !== 'string' || (id !== null && v.stream !== id)) return;
    if (v.data.length > 88_000) return fail('TUI render chunk is too large');
    try { receive({ stream: v.stream, data: b64.decode(v.data) }); } catch { fail('Invalid TUI render data'); }
  });
  const offClose = rpc.on('tui.closed', (p) => {
    const v = p as { stream?: unknown; reason?: unknown };
    if (typeof v.stream === 'string') receive({ stream: v.stream, reason: typeof v.reason === 'string' ? v.reason : 'TUI connection closed' });
  });
  signal?.addEventListener('abort', close, { once: true });
  if (signal?.aborted) close();
  if (closed) throw new Error('TUI attach cancelled');
  try {
    const result = await rpc.request<{ stream: string; client_id: string; features: string[]; protocol: number }>('tui.attach', { protocol });
    id = result.stream;
    if (closed) {
      if (!rpc.closed) void rpc.request('tui.detach', { stream: id }).catch(() => {});
      throw new Error('TUI attach cancelled');
    }
    if (typeof id !== 'string' || result.protocol !== protocol || !Array.isArray(result.features)) throw new Error('Invalid TUI attach response');
    return {
      id, clientId: result.client_id, features: result.features,
      start() {
        if (closed || started) return;
        started = true;
        const events = pending; pending = []; queued = 0;
        for (const e of events) receive(e);
      },
      async send(data) {
        if (closed || rpc.closed) throw new Error('TUI is disconnected');
        if (data.byteLength > 512 * 1024) throw new Error('TUI input is too large');
        await rpc.request('tui.send', { stream: id, data: b64.encode(data) });
      },
      close,
    };
  } catch (e) { close(); throw e; }
}
