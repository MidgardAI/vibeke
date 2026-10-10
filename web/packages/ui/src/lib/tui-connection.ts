import type { Clock, HostConnection, TimerHandle, TuiStream } from '@vibeke/core';

export type TuiState = { kind: 'connecting' | 'connected' | 'reconnecting' | 'offline' | 'blocked'; message: string; retryAt?: number };
export interface TuiTransportRuntime {
  connected(id: string, features: string): void;
  disconnected(reason: string): void;
  receive(data: Uint8Array): void;
  outgoing(): Uint8Array;
}
type Host = Pick<HostConnection, 'getSnapshot' | 'subscribe' | 'reconnectNow'> & Partial<Pick<HostConnection, 'openTui'>>;

/** One render attachment. Reconnect never recreates the Rust UI and never resends input. */
export class TuiConnection {
  private generation = 0;
  private disposed = false;
  private blocked = false;
  private broken = false;
  private attempt: AbortController | null = null;
  private stream: TuiStream | null = null;
  private timer: TimerHandle | null = null;
  private stable: TimerHandle | null = null;
  private retries = 0;
  private inFlight = 0;
  private off: (() => void) | undefined;

  constructor(private readonly o: {
    host: Host; runtime: TuiTransportRuntime; protocol: number; clock: Clock; random?: () => number;
    state(s: TuiState): void; wake(): void; notice(message: string): void; crashed(error: unknown): void;
  }) {}

  start(): void {
    this.off = this.o.host.subscribe(() => this.hostChanged());
    this.hostChanged();
  }
  private clearTimer(): void {
    if (this.timer !== null) this.o.clock.clearTimeout(this.timer);
    this.timer = null;
  }
  private detach(reason: string): void {
    ++this.generation;
    this.attempt?.abort(); this.attempt = null;
    const stream = this.stream; this.stream = null;
    stream?.close();
    this.inFlight = 0;
    if (this.stable !== null) this.o.clock.clearTimeout(this.stable);
    this.stable = null;
    if (!this.broken) {
      try { this.o.runtime.disconnected(reason); }
      catch (error) { this.crash(error); return; }
    }
    this.o.wake();
  }
  private hostChanged(): void {
    if (this.disposed || this.blocked) return;
    const state = this.o.host.getSnapshot();
    if (state.status !== 'online') {
      this.clearTimer();
      const terminal = ['revoked', 'expired', 'ticket_expired', 'incompatible'].includes(state.status);
      const message = terminal ? (state.status === 'revoked' ? 'Device access was revoked. Pair again to connect.' : 'Host access needs attention. Open host settings.') : 'Waiting for the host…';
      if (this.stream || this.attempt) {
        if (this.inFlight) this.o.notice('Connection interrupted. Some input may not have arrived. Nothing will be replayed.');
        this.detach(message);
      }
      this.o.state({ kind: terminal ? 'blocked' : 'offline', message });
      return;
    }
    if (!this.blocked && !this.stream && !this.attempt && this.timer === null) this.connect();
  }
  private connect(): void {
    if (this.disposed || this.blocked || this.attempt || this.stream) return;
    const state = this.o.host.getSnapshot();
    if (state.status !== 'online') return this.hostChanged();
    if (state.info?.scope !== 'full' || (state.info.kind ?? 'device') !== 'device' || state.info.limit) return this.fail('The terminal needs a paired device with full host access.');
    if (!state.info.features.includes('wasm_tui') || !this.o.host.openTui) return this.fail('Update Vibeke on this host to use the browser terminal.');
    const generation = ++this.generation;
    const controller = new AbortController(); this.attempt = controller;
    this.o.state({ kind: 'connecting', message: 'Connecting…' });
    void this.o.host.openTui(this.o.protocol, {
      frame: (bytes) => {
        if (!this.current(generation)) return;
        try { this.o.runtime.receive(bytes); this.o.wake(); }
        catch (error) { this.runtimeError(error); }
      },
      closed: (reason) => { if (this.current(generation)) this.retry(reason); },
    }, controller.signal).then((stream) => {
      if (!this.current(generation)) { stream.close(); return; }
      this.attempt = null; this.stream = stream;
      try { this.o.runtime.connected(stream.clientId, JSON.stringify(stream.features)); }
      catch (error) { this.runtimeError(error); return; }
      stream.start(); // May synchronously deliver a buffered close/error. Do not overwrite it.
      if (!this.current(generation) || this.stream !== stream) return;
      this.o.state({ kind: 'connected', message: 'Connected' });
      this.stable = this.o.clock.setTimeout(() => { this.stable = null; this.retries = 0; }, 10_000);
      this.o.wake();
    }).catch((error) => {
      if (!this.current(generation)) return;
      const kind = (error as { kind?: string })?.kind;
      if (['unsupported', 'forbidden', 'invalid_params', 'method_not_found'].includes(kind ?? '')) {
        this.fail(kind === 'unsupported' ? 'The host and browser terminal versions do not match. Update the host and reload this app.' : String(error));
      } else this.retry('The terminal connection failed. Retrying…');
    });
  }
  private current(generation: number): boolean { return !this.disposed && this.generation === generation; }
  private retry(message: string): void {
    if (this.inFlight) this.o.notice('Connection interrupted. Some input may not have arrived. Nothing will be replayed.');
    this.detach(message);
    this.clearTimer();
    if (this.disposed || this.blocked) return;
    const delay = Math.min(30_000, 500 * 2 ** Math.min(this.retries++, 6) * (0.8 + 0.4 * (this.o.random?.() ?? Math.random())));
    this.o.state({ kind: 'reconnecting', message: 'Reconnecting…', retryAt: this.o.clock.now() + delay });
    this.timer = this.o.clock.setTimeout(() => { this.timer = null; this.connect(); }, delay);
  }
  /** Bound both buffered WASM bytes and in-flight batches; RPC ordering preserves input order. */
  flush(): void {
    const stream = this.stream;
    if (!stream || this.disposed || this.blocked) return;
    const generation = this.generation;
    try {
      while (this.inFlight < 4) {
        const bytes = this.o.runtime.outgoing();
        if (!bytes.length) break;
        ++this.inFlight;
        void stream.send(bytes).catch(() => {
          if (this.current(generation)) this.retry('Terminal input could not be acknowledged.');
        }).finally(() => {
          if (this.current(generation)) { --this.inFlight; this.o.wake(); }
        });
      }
    } catch (error) { this.runtimeError(error); }
  }
  private runtimeError(error: unknown): void {
    // Rust returns strings for expected validation/queue failures. A JS/WASM
    // exception indicates a damaged runtime, which must never be used again.
    if (typeof error === 'string') this.fail(error);
    else this.crash(error);
  }
  private crash(error: unknown): void {
    if (this.broken) return;
    this.stop();
    this.o.crashed(error);
  }
  stop(): void {
    this.broken = true; this.blocked = true;
    this.clearTimer(); this.detach('Terminal stopped');
  }
  fail(message: string): void {
    this.blocked = true;
    this.clearTimer();
    this.detach(message);
    if (!this.broken) this.o.state({ kind: 'blocked', message });
  }
  reconnect(): void {
    if (this.broken) return;
    this.blocked = false; this.retries = 0;
    this.clearTimer(); this.detach('Reconnecting…');
    this.o.host.reconnectNow(); this.hostChanged();
  }
  dispose(): void {
    this.disposed = true; this.off?.(); this.clearTimer(); this.detach('Disconnected');
  }
}
