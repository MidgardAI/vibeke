import { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { ImageAddon } from '@xterm/addon-image';
import type { TuiStream } from '@vibeke/core';
import { useApp, useHosts } from '../app/hooks';
import { Button } from '../components/ui';
import { navigate } from '../router';
import { createWasmLoader } from '../lib/wasm-loader';
import '@xterm/xterm/css/xterm.css';

/** The small wasm-bindgen boundary; the Rust TUI owns screen state and keybindings. */
export interface BrowserTui {
  connected(clientId: string, features: string): void;
  disconnected(reason: string): void;
  receive(data: Uint8Array): void;
  input(data: Uint8Array): void;
  paste(text: string): void;
  key(name: string, mods: number, repeat: boolean, release: boolean): boolean;
  resize(cols: number, rows: number, cellW: number, cellH: number): void;
  focus(focused: boolean): void;
  action(name: string): void;
  toast(message: string): void;
  quit_reason(): string | undefined;
  render(): Uint8Array;
  outgoing(): Uint8Array;
  take_url(): string | undefined;
  take_download(): [string, Uint8Array] | undefined;
  take_clipboard(): string | undefined;
  free(): void;
}
interface TuiModule {
  default(): Promise<unknown>;
  render_protocol(): number;
  BrowserTui: new (label: string, host: string) => BrowserTui;
}

const loadTuiModule = createWasmLoader<TuiModule>((url) => import(/* @vite-ignore */ url));

export default function TuiScreen({ host }: { host: string }) {
  const app = useApp();
  const hosts = useHosts();
  const h = hosts.find((h) => h.record.host_id === host);
  const mount = useRef<HTMLDivElement>(null);
  const runtime = useRef<{ tui: BrowserTui; terminal: Terminal } | null>(null);
  const [status, setStatus] = useState('Loading TUI…');
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const [accessible, setAccessible] = useState(false);
  const moduleUrl = app.platform.tui?.moduleUrl;
  const conn = app.conn(host);
  const label = h?.record.name ?? host;

  useEffect(() => {
    const element = mount.current;
    if (!element || !moduleUrl || !conn?.openTui) return;
    let disposed = false;
    let tui: BrowserTui | null = null;
    let stream: TuiStream | null = null;
    let attempt: AbortController | null = null;
    let module: TuiModule | null = null;
    let frame = 0;
    let writing = false;
    let sending = false;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    const encoder = new TextEncoder();
    const terminal = new Terminal({
      fontFamily: '"SFMono-Regular", Consolas, "Liberation Mono", monospace', fontSize: 13,
      scrollback: 0, cursorBlink: true, screenReaderMode: accessible, allowProposedApi: true,
      theme: { background: '#0f1012', foreground: '#d7d9df' },
      linkHandler: { activate(_event, uri) { if (/^https?:\/\//i.test(uri)) app.platform.openExternal(uri); } },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.loadAddon(new ImageAddon({ enableSizeReports: false, sixelSupport: false }));
    terminal.open(element);
    terminal.write('\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h');
    const size = () => {
      if (disposed || element.clientWidth === 0 || element.clientHeight === 0) return;
      fit.fit();
      tui?.resize(terminal.cols, terminal.rows, Math.max(1, Math.round(element.clientWidth / terminal.cols)), Math.max(1, Math.round(element.clientHeight / terminal.rows)));
    };
    const observer = new ResizeObserver(size);
    observer.observe(element);
    const onData = terminal.onData((text) => tui?.input(encoder.encode(text)));
    const onBinary = terminal.onBinary((text) => tui?.input(Uint8Array.from(text, (c) => c.charCodeAt(0))));
    // Keep IME and ordinary text on xterm's composition path. Modified keys retain their
    // logical identity instead of collapsing Ctrl+Shift+P into the bytes for Ctrl+P.
    terminal.attachCustomKeyEventHandler((event) => {
      if (!tui || event.isComposing || event.key === 'Process' || event.key === 'Dead') return true;
      if (event.metaKey || (event.ctrlKey && event.shiftKey && ['C', 'V'].includes(event.key.toUpperCase()))) return true;
      if (!(event.ctrlKey || event.altKey || (event.shiftKey && event.key === 'Enter'))) return true;
      if (event.type === 'keypress') return false;
      const mods = (event.shiftKey ? 1 : 0) | (event.altKey ? 2 : 0) | (event.ctrlKey ? 4 : 0);
      const handled = tui.key(event.key, mods, event.repeat, event.type === 'keyup');
      if (handled) event.preventDefault();
      return !handled;
    });
    const focus = () => tui?.focus(document.visibilityState === 'visible' && document.hasFocus());
    window.addEventListener('focus', focus);
    window.addEventListener('blur', focus);
    document.addEventListener('visibilitychange', focus);

    const disconnect = (reason: string) => {
      attempt?.abort(); attempt = null;
      stream?.close(); stream = null;
      tui?.disconnected(reason);
      if (!disposed) setStatus(reason);
    };
    const retryConnection = (reason: string) => {
      disconnect(reason);
      if (!disposed) {
        setError(reason);
        clearTimeout(reconnect);
        reconnect = setTimeout(() => connect(), 1500);
      }
    };
    const connect = () => {
      if (disposed || !module || !tui || attempt || stream) return;
      const state = conn.getSnapshot();
      if (state.status !== 'online') { setStatus(state.status === 'revoked' ? 'Device access was revoked' : 'Waiting for host…'); return; }
      if (state.info?.scope !== 'full' || (state.info.kind ?? 'device') !== 'device' || state.info.limit) {
        setError('The TUI needs a paired device with full host access.'); return;
      }
      if (!state.info.features.includes('wasm_tui')) { setError('This host does not support the browser TUI.'); return; }
      const controller = new AbortController(); attempt = controller;
      setStatus('Connecting TUI…');
      void conn.openTui!(module.render_protocol(), {
        frame: (bytes) => {
          if (disposed || controller.signal.aborted) return;
          try { tui?.receive(bytes); }
          catch (e) { console.error('Browser TUI receive failed', e); throw e; }
        },
        closed: (reason) => { if (!disposed && !controller.signal.aborted) retryConnection(reason); },
      }, controller.signal).then((opened) => {
        if (disposed || controller.signal.aborted) { opened.close(); return; }
        attempt = null; stream = opened;
        tui!.connected(opened.clientId, JSON.stringify(opened.features));
        size(); focus(); opened.start();
        setStatus('Connected'); setError(null);
      }).catch((e) => {
        if (disposed || controller.signal.aborted) return;
        disconnect('TUI unavailable'); setError(String(e));
      });
    };
    const offHost = conn.subscribe(() => {
      if (conn.getSnapshot().status !== 'online') disconnect(conn.getSnapshot().status === 'revoked' ? 'Device access was revoked' : 'Waiting for host…');
      else connect();
    });
    const animate = () => {
      if (disposed || !tui) return;
      try {
        // xterm's write callback provides backpressure. Rust coalesces screen changes while
        // rendering catches up; input and render-stream ACKs continue independently.
        if (!writing) {
          const bytes = tui.render();
          if (bytes.length) { writing = true; terminal.write(bytes, () => { writing = false; }); }
        }
        if (stream && !sending) {
          const bytes = tui.outgoing();
          if (bytes.length) {
            sending = true;
            const current = stream;
            void current.send(bytes).catch((e) => { if (stream === current) retryConnection(String(e)); }).finally(() => { sending = false; });
          }
        }
        const url = tui.take_url();
        if (url && /^https?:\/\//i.test(url)) app.platform.openExternal(url);
        const download = tui.take_download();
        if (download) {
          const url = URL.createObjectURL(new Blob([download[1] as BlobPart], { type: 'image/png' }));
          const a = document.createElement('a'); a.href = url; a.download = download[0]; a.click();
          setTimeout(() => URL.revokeObjectURL(url), 60_000);
        }
        const copied = tui.take_clipboard();
        if (copied !== undefined) void app.platform.clipboard.writeText(copied).catch(() => tui?.toast('Clipboard permission was denied. Use browser copy.'));
        if (tui.quit_reason()) { navigate({ name: 'settings', section: 'system' }); return; }
      } catch (e) { setError(String(e)); disconnect('TUI stopped'); return; }
      frame = requestAnimationFrame(animate);
    };
    void (async () => {
      try {
        // Absolute URLs also keep Vite's dev import helper from adding ?import to
        // the generated public module. Its relative WASM URL stays in the same build.
        const moduleLocation = new URL(moduleUrl, window.location.href).href;
        module = await loadTuiModule(moduleLocation);
        if (disposed) return;
        tui = new module.BrowserTui(label, host);
        runtime.current = { tui, terminal };
        size(); connect(); terminal.focus(); frame = requestAnimationFrame(animate);
      } catch (e) { if (!disposed) { setError(String(e)); setStatus('TUI unavailable'); } }
    })();
    return () => {
      disposed = true; runtime.current = null;
      clearTimeout(reconnect); cancelAnimationFrame(frame); offHost(); disconnect('Disconnected');
      observer.disconnect(); onData.dispose(); onBinary.dispose();
      window.removeEventListener('focus', focus); window.removeEventListener('blur', focus);
      document.removeEventListener('visibilitychange', focus);
      terminal.dispose(); tui?.free(); tui = null;
    };
  }, [app, host, conn, moduleUrl, retry]);

  const action = (name: string) => { runtime.current?.tui.action(name); runtime.current?.terminal.focus(); };
  return (
    <div className="flex h-full min-h-0 flex-col bg-[#0f1012] text-[#d7d9df]" data-testid="browser-tui">
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-white/10 px-3 py-2 text-xs">
        <Button onClick={() => navigate({ name: 'settings', section: 'system' })}>Back</Button>
        <strong>{label} · TUI</strong><span role="status" className="mr-auto text-gray-400">{status}</span>
        <Button onClick={() => action('command_palette')}>Commands</Button>
        <Button onClick={() => action('inbox')}>Inbox</Button>
        <Button onClick={() => { void app.platform.clipboard.readText?.().then((text) => { runtime.current?.tui.paste(text); runtime.current?.terminal.focus(); }).catch(() => setError('Clipboard permission was denied. Use browser paste.')); }}>Paste</Button>
        <Button aria-pressed={accessible} onClick={() => {
          setAccessible(!accessible);
          if (runtime.current) {
            const terminal = runtime.current.terminal;
            terminal.options.screenReaderMode = !accessible;
            // Screen reader mode allows browser edits in the helper textarea. Those edits
            // were already sent; do not carry them into the next IME composition.
            if (terminal.textarea) terminal.textarea.value = '';
            terminal.focus();
          }
        }}>Screen reader</Button>
        <Button onClick={() => setRetry((n) => n + 1)}>Reconnect</Button>
      </div>
      {(!moduleUrl || !conn?.openTui) && <div className="p-4">The TUI is unavailable in this app.</div>}
      {error && <div role="alert" className="shrink-0 bg-red-950/60 px-3 py-2 text-sm">{error}</div>}
      <div ref={mount} className="min-h-0 min-w-0 flex-1 overflow-hidden p-1" aria-label="Vibeke terminal interface" />
      <div className="shrink-0 border-t border-white/10 px-3 py-1 text-xs text-gray-400">Ctrl+B opens the prefix menu. Ctrl+B, then : opens commands. Browser shortcuts may take priority.</div>
    </div>
  );
}
