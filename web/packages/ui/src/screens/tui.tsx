import { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { ImageAddon } from '@xterm/addon-image';
import { MoreHorizontal } from 'lucide-react';
import { useApp, useHosts, usePrefs } from '../app/hooks';
import { Button } from '../components/ui';
import { Dialog } from '../components/dialog';
import { formatRoute, navigate } from '../router';
import { createWasmLoader, publishedTuiModule } from '../lib/wasm-loader';
import { createTuiKeyboard } from '../lib/tui-keyboard';
import { TuiConnection, type TuiState } from '../lib/tui-connection';
import { TuiDriver } from '../lib/tui-driver';
import { BROWSER_TUI_API, type BrowserTui } from '../lib/tui-api';
import '@xterm/xterm/css/xterm.css';


interface TuiModule {
  default(): Promise<unknown>;
  render_protocol(): number;
  browser_api(): number;
  BrowserTui: new (label: string, host: string) => BrowserTui;
}
const loadTuiModule = createWasmLoader<TuiModule>((url) => import(/* @vite-ignore */ url), publishedTuiModule);
const DARK = { background: '#0f1012', foreground: '#cdd6f4', cursor: '#cdd6f4', selectionBackground: '#45475a' };
const LIGHT = { background: '#eff1f5', foreground: '#4c4f69', cursor: '#4c4f69', selectionBackground: '#bcc0cc' };
interface Runtime { tui: BrowserTui; terminal: Terminal; connection: TuiConnection; wake(): void; size(): void; appearance(): void; run(fn: (t: BrowserTui) => void): void }

export default function TuiScreen({ host, workspace, pane }: { host: string; workspace?: string; pane?: string }) {
  const app = useApp();
  const hosts = useHosts();
  const prefs = usePrefs();
  const preferences = useRef(prefs); preferences.current = prefs;
  const h = hosts.find((h) => h.record.host_id === host);
  const mount = useRef<HTMLDivElement>(null);
  const runtime = useRef<Runtime | null>(null);
  const [state, setState] = useState<TuiState>({ kind: 'connecting', message: 'Loading terminal…' });
  const [menu, setMenu] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [link, setLink] = useState<string | null>(null);
  const [download, setDownload] = useState<{ url: string; name: string } | null>(null);
  const [copyText, setCopyText] = useState<string | null>(null);
  const [manualPaste, setManualPaste] = useState('');
  const [pasteFallback, setPasteFallback] = useState(false);
  const [fontDraft, setFontDraft] = useState(String(prefs.termFont));
  const [crashed, setCrashed] = useState(false);
  const [pendingRecovery, setPendingRecovery] = useState(false);
  const moduleUrl = app.platform.tui?.moduleUrl;
  const conn = app.conn(host);
  const label = h?.record.name ?? host;
  const labelRef = useRef(label); labelRef.current = label;
  const locked = useRef(app.locked.get());
  const leave = () => { app.prefs.patch({ preferredTuiHost: null }); navigate({ name: 'settings', section: 'system' }); };

  useEffect(() => {
    if (!moduleUrl || !conn?.openTui || !mount.current) {
      setState({ kind: 'blocked', message: !conn ? 'This host is not paired. Open host settings to pair it.' : 'The browser terminal is unavailable in this app.' });
      return;
    }
    const element = mount.current;
    let disposed = false;
    let tui: BrowserTui | null = null;
    let connection: TuiConnection | null = null;
    let driver: TuiDriver | null = null;
    let frame = 0;
    let writing = false;
    let failed = false;
    let lastSize = '';
    let visible = document.visibilityState === 'visible' && !app.locked.get();
    const encoder = new TextEncoder();
    const theme = window.matchMedia('(prefers-color-scheme: light)');
    const light = () => preferences.current.theme === 'light' || (preferences.current.theme === 'system' && theme.matches);
    const terminal = new Terminal({
      fontFamily: '"SFMono-Regular", Consolas, "Liberation Mono", monospace', fontSize: preferences.current.termFont,
      scrollback: 0, cursorBlink: true, screenReaderMode: preferences.current.tuiScreenReader,
      macOptionIsMeta: preferences.current.tuiOptionMeta, allowProposedApi: true,
      theme: light() ? LIGHT : DARK,
      linkHandler: { activate(_event, uri) { if (/^https?:\/\//i.test(uri)) app.platform.openExternal(uri); } },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.loadAddon(new ImageAddon({ enableSizeReports: false, sixelSupport: false }));
    terminal.open(element);
    terminal.write('\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h');
    const wake = () => { if (!disposed && !failed) driver?.wake(); };
    const fatal = (error: unknown) => {
      if (failed || disposed) return;
      failed = true;
      driver?.dispose();
      try { connection?.stop(); } catch { /* A trapped WASM instance cannot be reused. */ }
      setCrashed(true); setState({ kind: 'blocked', message: 'The terminal stopped. Reload the terminal to reconnect. Host processes are still running.' });
    };
    const update = (fn: (t: BrowserTui) => void, system = false) => {
      if (!tui || failed || disposed || (!system && locked.current)) return;
      try { fn(tui); wake(); } catch (error) { fatal(error); }
    };
    const size = () => {
      if (disposed || failed || !element.clientWidth || !element.clientHeight) return;
      fit.fit();
      const screen = element.querySelector('.xterm-screen')?.getBoundingClientRect();
      if (!screen?.width || !screen.height) return;
      const dpr = window.devicePixelRatio || 1;
      const dimensions = [terminal.cols, terminal.rows, Math.max(1, Math.round(screen.width / terminal.cols * dpr)), Math.max(1, Math.round(screen.height / terminal.rows * dpr)), Math.round(dpr * 100)] as const;
      const key = dimensions.join(',');
      if (tui && lastSize !== key) { lastSize = key; update((t) => t.resize(...dimensions), true); }
    };
    const appearance = () => {
      terminal.options.theme = light() ? LIGHT : DARK;
      terminal.options.fontSize = preferences.current.termFont;
      terminal.options.macOptionIsMeta = preferences.current.tuiOptionMeta;
      if (terminal.options.screenReaderMode !== preferences.current.tuiScreenReader) {
        terminal.options.screenReaderMode = preferences.current.tuiScreenReader;
        if (terminal.textarea) terminal.textarea.value = '';
      }
      update((t) => t.appearance(light()), true);
      size();
    };
    const paint = () => {
      if (disposed || failed || !visible || writing || frame || !tui?.dirty()) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        if (disposed || failed || !visible || !tui) return;
        try {
          const bytes = tui.render();
          if (bytes.length) {
            writing = true;
            terminal.write(bytes, () => { writing = false; paint(); });
          }
          wake(); // Drawing may enqueue view hints. Flush without waiting for another paint.
        } catch (error) { fatal(error); }
      });
    };
    const keyboard = createTuiKeyboard((...args) => {
      if (!tui || failed || locked.current) return false;
      try { const result = tui.key(...args); wake(); return result; } catch (error) { fatal(error); return true; }
    }, { mac: /Mac|iPhone|iPad/.test(navigator.platform), optionAsMeta: () => preferences.current.tuiOptionMeta });
    terminal.attachCustomKeyEventHandler((event) => {
      if (event.type === 'keydown' && event.ctrlKey && event.shiftKey && event.code === 'Period') {
        event.preventDefault(); setMenu(true); return false;
      }
      return keyboard.handle(event);
    });
    const data = terminal.onData((text) => update((t) => t.input(encoder.encode(text))));
    const binary = terminal.onBinary((text) => update((t) => t.input(Uint8Array.from(text, (c) => c.charCodeAt(0)))));
    const syncVisibility = () => {
      if (app.locked.get()) keyboard.releaseAll();
      locked.current = app.locked.get();
      const next = document.visibilityState === 'visible' && !locked.current;
      if (!next) keyboard.releaseAll();
      if (tui && !failed) {
        try {
          tui.focus(next && document.hasFocus() && document.activeElement === terminal.textarea);
          if (next !== visible) { tui.visible(next); lastSize = ''; }
        } catch (error) { fatal(error); }
      }
      visible = next;
      if (visible) size();
      wake();
    };
    const blur = () => { keyboard.releaseAll(); syncVisibility(); };
    const observer = new ResizeObserver(size); observer.observe(element);
    terminal.textarea?.addEventListener('focus', syncVisibility);
    terminal.textarea?.addEventListener('blur', blur);
    window.addEventListener('focus', syncVisibility);
    window.addEventListener('blur', blur);
    window.addEventListener('resize', size);
    window.visualViewport?.addEventListener('resize', size);
    document.addEventListener('visibilitychange', syncVisibility);
    theme.addEventListener('change', appearance);
    const offLock = app.locked.subscribe(syncVisibility);
    void document.fonts.ready.then(() => { if (!disposed) size(); });
    // Window resize covers browser zoom; a resolution query also catches moving between displays.
    let resolution: MediaQueryList | null = null;
    const dprChanged = () => { resolution?.removeEventListener('change', dprChanged); resolution = matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`); resolution.addEventListener('change', dprChanged); size(); };
    dprChanged();
    const oldTitle = document.title; document.title = `${labelRef.current} · Terminal`;
    setCrashed(false);
    setPendingRecovery(false);
    setState({ kind: 'connecting', message: 'Loading terminal…' });
    void (async () => {
      try {
        const started = performance.now();
        const module = await loadTuiModule(new URL(moduleUrl, window.location.href).href);
        if (disposed) return;
        if (module.browser_api?.() !== BROWSER_TUI_API) throw new Error('The terminal module and app versions differ. Reload the app to update both.');
        tui = new module.BrowserTui(labelRef.current, host);
        if (workspace || pane) tui.focus_target(workspace ?? '', pane ?? '');
        connection = new TuiConnection({ host: conn, runtime: tui, protocol: module.render_protocol(), clock: app.platform.clock,
          state: (value) => {
            if (disposed) return;
            if (value.kind === 'connected') { lastSize = ''; update((t) => t.visible(visible), true); size(); syncVisibility(); }
            if (!failed) setState(value);
          }, wake, crashed: fatal, notice: (message) => { if (!disposed) setNotice(message); },
        });
        driver = new TuiDriver({ clock: app.platform.clock, tick: () => tui!.tick(), flush: () => connection!.flush(), paint, failed: fatal,
          effects: () => {
            const url = tui!.take_url();
            if (url && /^https?:\/\//i.test(url)) setLink(url);
            const file = tui!.take_download();
            if (file) {
              const url = URL.createObjectURL(new Blob([file[1] as BlobPart], { type: 'image/png' }));
              setDownload((old) => { if (old) URL.revokeObjectURL(old.url); return { url, name: file[0] }; });
            }
            const copied = tui!.take_clipboard();
            if (copied !== undefined) void app.platform.clipboard.writeText(copied).catch(() => {
              if (!disposed) { setCopyText(copied); setNotice('Clipboard access was denied. Open the browser menu to copy the text.'); }
            });
            if (tui!.quit_reason()) { app.prefs.patch({ preferredTuiHost: null }); navigate({ name: 'settings', section: 'system' }); }
          },
        });
        runtime.current = { tui, terminal, connection, wake, size, appearance, run: update };
        appearance(); size(); connection.start();
        if (visible && !document.querySelector('[role="dialog"]') && (!document.activeElement || document.activeElement === document.body)) terminal.focus();
        performance.measure('vibeke-tui-initialize', { start: started, end: performance.now() });
        wake();
      } catch (error) {
        if (!disposed) {
          if (tui) fatal(error);
          else {
            setCrashed(true);
            setPendingRecovery(String(error).includes('Pending TUI operations could not be read'));
            setState({ kind: 'blocked', message: `The terminal could not load. ${String(error)}` });
          }
        }
      }
    })();
    return () => {
      disposed = true; runtime.current = null;
      driver?.dispose(); cancelAnimationFrame(frame);
      try { connection?.dispose(); } catch { /* Trapped WASM is freed below. */ }
      offLock(); observer.disconnect(); data.dispose(); binary.dispose();
      terminal.textarea?.removeEventListener('focus', syncVisibility);
      terminal.textarea?.removeEventListener('blur', blur);
      window.removeEventListener('focus', syncVisibility); window.removeEventListener('blur', blur); window.removeEventListener('resize', size);
      window.visualViewport?.removeEventListener('resize', size);
      document.removeEventListener('visibilitychange', syncVisibility); theme.removeEventListener('change', appearance);
      resolution?.removeEventListener('change', dprChanged);
      document.title = oldTitle;
      terminal.dispose(); try { tui?.free(); } catch { /* Already trapped. */ } tui = null;
    };
  }, [app, host, workspace, pane, conn, moduleUrl]);

  useEffect(() => { runtime.current?.appearance(); }, [prefs.theme, prefs.termFont, prefs.tuiScreenReader, prefs.tuiOptionMeta]);
  useEffect(() => { setFontDraft(String(prefs.termFont)); }, [prefs.termFont]);
  const saveFont = () => { const n = Number(fontDraft); if (Number.isInteger(n) && n >= 8 && n <= 24) app.prefs.patch({ termFont: n }); else setFontDraft(String(prefs.termFont)); };
  useEffect(() => () => { if (download) URL.revokeObjectURL(download.url); }, [download]);
  const focus = () => { requestAnimationFrame(() => runtime.current?.terminal.focus()); };
  const action = (name: string) => { runtime.current?.run((t) => t.action(name)); setMenu(false); focus(); };
  const paste = (text: string) => { runtime.current?.run((t) => t.paste(text)); setManualPaste(''); setPasteFallback(false); setMenu(false); focus(); };
  const copy = async (text: string) => {
    try { await app.platform.clipboard.writeText(text); setNotice('Copied'); setCopyText(null); }
    catch { setCopyText(text); setNotice('Select the text in the browser menu and use your browser’s Copy command.'); }
  };
  const retry = () => { if (crashed) window.location.reload(); else runtime.current?.connection.reconnect(); };
  const connectionIssue = state.kind !== 'connected';
  return (
    <div className="relative h-full min-h-0 w-full" data-testid="browser-tui">
      <div ref={mount} className="h-full min-h-0 min-w-0 overflow-hidden" aria-label={`${label} terminal`} />
      <span className="sr-only" role="status" aria-live="polite">{state.message}</span>
      <span className="sr-only" aria-live="polite" aria-atomic="true">{notice}{link ? ' Link ready. Use Open link.' : ''}{download ? ' Screenshot ready. Use Save screenshot.' : ''}</span>
      <button type="button" aria-label="Browser menu" aria-haspopup="dialog" aria-expanded={menu} title="Browser menu (Ctrl+Shift+.)"
        className="absolute bottom-1 right-1 z-20 flex h-6 w-7 items-center justify-center rounded border border-border bg-surface text-muted shadow-sm hover:text-fg focus-visible:outline-2 focus-visible:outline-accent pointer-coarse:h-10 pointer-coarse:w-10"
        onClick={() => setMenu(true)}><MoreHorizontal className="size-4" /></button>
      {(connectionIssue || notice || link || download || !prefs.tuiHintDismissed) && <div className="absolute left-1/2 top-3 z-20 flex max-w-[calc(100%-2rem)] -translate-x-1/2 flex-wrap items-center gap-2 rounded-lg border border-border bg-surface/95 px-3 py-2 text-sm text-fg shadow-lg" data-testid="terminal-notice">
        {connectionIssue ? <><span>{state.message}</span><Button size="sm" onClick={retry}>{crashed ? 'Reload terminal' : 'Retry'}</Button>{pendingRecovery && <Button size="sm" onClick={() => setMenu(true)}>Review saved operations</Button>}<Button size="sm" onClick={leave}>Host settings</Button></> : <>
          {notice && <span>{notice}</span>}
          {link && <a className="underline" href={link} target="_blank" rel="noopener noreferrer" onClick={() => setLink(null)}>Open link</a>}
          {download && <a className="underline" href={download.url} download={download.name}>Save screenshot</a>}
          {!notice && !link && !download && <span>Ctrl+B opens the terminal menu. Ctrl+Shift+. opens browser controls.</span>}
          <Button size="sm" aria-label="Dismiss terminal notice" onClick={() => { setNotice(null); setLink(null); setDownload(null); app.prefs.patch({ tuiHintDismissed: true }); }}>Dismiss</Button>
        </>}
      </div>}
      <Dialog open={menu} onClose={() => setMenu(false)} label="Browser terminal controls"
        className="fixed inset-0 z-50 flex items-end justify-end bg-black/25 p-3"
        panelClassName="max-h-[90dvh] w-full max-w-sm space-y-4 overflow-y-auto rounded-xl border border-border bg-surface p-4 text-fg shadow-xl">
        <div className="flex items-center justify-between"><strong>{label}</strong><Button size="sm" onClick={() => setMenu(false)}>Close</Button></div>
        <p className="text-sm text-muted">{state.message}</p>
        {pendingRecovery && <div className="space-y-2 rounded border border-border p-3 text-sm">
          <p>Saved operation metadata cannot be read. Check task status on the host before discarding it. Discarding removes this tab’s recovery metadata for this host. It does not cancel or repeat host operations.</p>
          <Button onClick={() => {
            try { sessionStorage.removeItem(`vibeke-tui-pending:${host}`); window.location.reload(); }
            catch { setNotice('Browser storage could not be cleared. Check this site’s storage permissions.'); }
          }}>Discard saved operations and reload</Button>
        </div>}
        <div className="flex flex-wrap gap-2">
          <Button onClick={() => action('command_palette')}>Commands</Button><Button onClick={() => action('inbox')}>Inbox</Button>
          <Button onClick={() => {
            const target = runtime.current;
            const read = app.platform.clipboard.readText;
            if (!read) { setPasteFallback(true); return; }
            void read().then((text) => { if (target && runtime.current === target) paste(text); })
              .catch(() => { if (runtime.current === target) setPasteFallback(true); });
          }}>Paste</Button>
          <Button onClick={() => { const text = runtime.current?.terminal.getSelection(); if (text) void copy(text); else setNotice('Select terminal text first, or use the TUI copy mode.'); }}>Copy selection</Button>
          <Button onClick={() => { runtime.current?.run((t) => { const loc = JSON.parse(t.location()); void copy(new URL(formatRoute({ name: 'tui', host, workspace: loc.workspace ?? undefined, pane: loc.pane ?? undefined }), location.href).href); }); }}>Copy terminal link</Button>
        </div>
        {pasteFallback && <div className="space-y-2"><label className="block text-sm" htmlFor="tui-manual-paste">Paste here with your browser shortcut</label><textarea id="tui-manual-paste" className="w-full rounded border border-border bg-bg p-2" value={manualPaste} onChange={(e) => setManualPaste(e.target.value)} /><Button onClick={() => paste(manualPaste)}>Send paste</Button></div>}
        {copyText !== null && <div><label htmlFor="tui-copy-text">Text to copy</label><textarea id="tui-copy-text" readOnly className="w-full rounded border border-border bg-bg p-2" value={copyText} onFocus={(e) => e.target.select()} /></div>}
        <label className="flex items-center justify-between gap-3 text-sm">Font size<input aria-label="Terminal font size" type="number" min={8} max={24} className="w-16 rounded border border-border bg-bg p-1" value={fontDraft} onChange={(e) => setFontDraft(e.target.value)} onBlur={saveFont} onKeyDown={(e) => { if (e.key === 'Enter') saveFont(); }} /></label>
        <label className="flex items-center justify-between gap-3 text-sm">Theme<select aria-label="Terminal theme" className="rounded border border-border bg-bg p-1" value={prefs.theme} onChange={(e) => app.prefs.patch({ theme: e.target.value as 'light' | 'dark' | 'system' })}><option value="system">System</option><option value="dark">Dark</option><option value="light">Light</option></select></label>
        <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={prefs.tuiScreenReader} onChange={(e) => app.prefs.patch({ tuiScreenReader: e.target.checked })} />Screen reader support</label>
        <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={prefs.tuiOptionMeta} onChange={(e) => app.prefs.patch({ tuiOptionMeta: e.target.checked })} />Use Option as Alt on Mac</label>
        <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={prefs.preferredTuiHost === host} onChange={(e) => app.prefs.patch({ preferredTuiHost: e.target.checked ? host : null })} />Open this terminal when I launch the app</label>
        <p className="text-xs text-muted">Ctrl+B opens the prefix menu. Ctrl+B, then : opens commands. Shift-drag selects text in the browser. Other clients can control a pane’s size; focus the pane to request control.</p>
        <div className="flex flex-wrap gap-2"><Button onClick={() => { retry(); setMenu(false); }}>{crashed ? 'Reload terminal' : 'Reconnect'}</Button><Button onClick={leave}>Leave terminal</Button></div>
      </Dialog>
    </div>
  );
}
