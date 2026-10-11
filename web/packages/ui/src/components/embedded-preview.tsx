// A dev-server preview embedded as a native browser view (desktop, spec 06 B3.5). The shell draws
// the page in a layer above this window; this component draws the toolbar, keeps a placeholder
// whose rectangle it reports, and hides the layer (showing a still image instead) while a dialog,
// menu or the command palette is open.

import { useCallback, useEffect, useLayoutEffect, useRef, useState, type FormEvent } from 'react';
import { ArrowLeft, ArrowRight, Bug, ExternalLink, Globe, RefreshCw, X } from 'lucide-react';
import type { Preview } from '@vibeke/core';
import { useApp, useHost } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { useOverlayOpen } from '../lib/overlays';
import type { PreviewViewApi, PreviewViewState } from '../platform';
import { Button, IconButton, Notice } from './ui';

const TUNNEL_FEATURE = 'tunnel';
/** How long hiding the view waits for its snapshot before it hides anyway. */
const SNAPSHOT_WAIT_MS = 200;
/** Position check while shown (layout shifts without a resize, e.g. a sidebar toggle). */
const POSITION_POLL_MS = 250;

/** `localhost` in the system browser means this device, so loopback URLs never leave the app. */
export function isLoopbackUrl(url: string): boolean {
  let h: string;
  try {
    h = new URL(url).hostname.toLowerCase().replace(/\.$/, '');
  } catch {
    return false;
  }
  if (h.startsWith('[') && h.endsWith(']')) h = h.slice(1, -1);
  return h === 'localhost' || h.endsWith('.localhost') || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(h) || h === '0.0.0.0' || h === '::1' || h === '::' || /^::ffff:(7f[0-9a-f]{2}:[0-9a-f]{1,4}|127\.)/.test(h);
}

/** What the address field submits: a bare `localhost:3000/x` becomes `http://localhost:3000/x`. */
export function addressToUrl(text: string): string | null {
  const s = text.trim();
  if (!s) return null;
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(s) ? s : `http://${s}`;
  try {
    const u = new URL(withScheme);
    return u.protocol === 'http:' || u.protocol === 'https:' ? u.toString() : null;
  } catch {
    return null;
  }
}

export function EmbeddedPreview({ hostId, preview }: { hostId: string; preview: Preview }) {
  const app = useApp();
  const host = useHost(hostId);
  const api = app.platform.previewView;
  const info = host?.info;
  if (!api) return null;
  if (info && !info.features.includes(TUNNEL_FEATURE)) {
    return (
      <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-6">
        <Notice className="max-w-md">{t.embeddedPreview.noTunnel}</Notice>
        {!isLoopbackUrl(preview.url) && (
          <Button size="sm" variant="outline" icon={<ExternalLink />} onClick={() => app.platform.openExternal(preview.url)}>
            {t.embeddedPreview.openBrowser}
          </Button>
        )}
        <div className="break-all font-mono text-xs text-muted">{preview.url}</div>
      </div>
    );
  }
  if (!info) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center p-6">
        <Notice className="max-w-md">{t.embeddedPreview.waiting}</Notice>
      </div>
    );
  }
  const machine = preview.machine && preview.machine !== 'local' ? preview.machine : info.host_name;
  return <PreviewSurface key={`${hostId}/${preview.id}`} api={api} hostId={hostId} preview={preview} machine={machine} />;
}

function PreviewSurface({ api, hostId, preview, machine }: { api: PreviewViewApi; hostId: string; preview: Preview; machine: string }) {
  const app = useApp();
  const [view, setView] = useState<string | null>(null);
  const [state, setState] = useState<PreviewViewState | null>(null);
  const [openError, setOpenError] = useState<string | null>(null);
  const [address, setAddress] = useState(preview.url);
  const [editing, setEditing] = useState(false);
  const [shot, setShot] = useState<string | null>(null);
  const [covered, setCovered] = useState(false);
  const [pageShown, setPageShown] = useState(() => typeof document === 'undefined' || document.visibilityState === 'visible');
  const overlay = useOverlayOpen();
  const box = useRef<HTMLDivElement>(null);
  const initialUrl = useRef(preview.url);

  // One view per mounted preview; closed on unmount.
  useEffect(() => {
    let live = true;
    let id: string | null = null;
    setOpenError(null);
    api
      .open({ hostId, machine: preview.machine && preview.machine !== 'local' ? preview.machine : '', url: initialUrl.current })
      .then((r) => {
        if (!live) return api.close(r.view);
        id = r.view;
        setView(r.view);
      })
      .catch((e) => live && setOpenError(errorMessage(e)));
    return () => {
      live = false;
      if (id) api.close(id);
      setView(null);
    };
  }, [api, hostId, preview.machine]);

  useEffect(() => {
    if (!view) return;
    return api.onState((s) => {
      if (s.view !== view) return;
      setState(s);
    });
  }, [api, view]);

  useEffect(() => {
    if (!editing && state?.url) setAddress(state.url);
  }, [state?.url, editing]);

  useEffect(() => {
    const f = () => setPageShown(document.visibilityState === 'visible');
    document.addEventListener('visibilitychange', f);
    return () => document.removeEventListener('visibilitychange', f);
  }, []);

  // An overlay opened: take a still image first, then hide the native layer under it.
  useEffect(() => {
    if (!overlay) {
      setCovered(false);
      return;
    }
    if (!view) {
      setCovered(true);
      return;
    }
    let live = true;
    const wait = new Promise<null>((r) => setTimeout(() => r(null), SNAPSHOT_WAIT_MS));
    void Promise.race([api.snapshot(view), wait]).then((img) => {
      if (!live) return;
      if (img) setShot(img);
      setCovered(true);
    });
    return () => {
      live = false;
    };
  }, [api, overlay, view]);

  const hostOffline = state?.error === 'host_offline';
  const crashed = state?.error === 'crashed';
  const visible = !!view && !covered && !hostOffline && !crashed && pageShown;
  const last = useRef('');

  const report = useCallback(
    (force = false) => {
      const el = box.current;
      if (!view || !el) return;
      const r = el.getBoundingClientRect();
      const key = `${r.x},${r.y},${r.width},${r.height},${visible}`;
      if (!force && key === last.current) return;
      last.current = key;
      api.bounds(view, { x: r.x, y: r.y, width: r.width, height: r.height }, visible);
    },
    [api, view, visible],
  );

  useLayoutEffect(() => {
    report(true);
    if (visible && !overlay) setShot(null);
  }, [report, visible, overlay]);

  useEffect(() => {
    const el = box.current;
    if (!view || !el) return;
    const on = () => report();
    const ro = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(on) : null;
    ro?.observe(el);
    window.addEventListener('resize', on);
    window.addEventListener('scroll', on, true);
    const poll = visible ? setInterval(on, POSITION_POLL_MS) : null;
    return () => {
      ro?.disconnect();
      window.removeEventListener('resize', on);
      window.removeEventListener('scroll', on, true);
      if (poll) clearInterval(poll);
    };
  }, [report, view, visible]);

  const go = (p: { url?: string; action?: 'back' | 'forward' | 'reload' | 'stop' }) => {
    if (!view) return;
    api.go(view, p).catch((e) => app.toast(errorMessage(e), 'error'));
  };
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const url = addressToUrl(address);
    if (!url) return;
    setEditing(false);
    (document.activeElement as HTMLElement | null)?.blur();
    go({ url });
  };

  const current = state?.url || preview.url;
  const loading = state?.loading ?? false;
  const external = !isLoopbackUrl(current);
  const s = t.embeddedPreview;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div role="toolbar" aria-label={s.toolbar} className="flex h-10 shrink-0 items-center gap-1 border-b border-border px-2">
        <IconButton label={s.back} disabled={!state?.canGoBack} onClick={() => go({ action: 'back' })}>
          <ArrowLeft />
        </IconButton>
        <IconButton label={s.forward} disabled={!state?.canGoForward} onClick={() => go({ action: 'forward' })}>
          <ArrowRight />
        </IconButton>
        <IconButton label={loading ? s.stop : s.reload} disabled={!view} onClick={() => go({ action: loading ? 'stop' : 'reload' })}>
          {loading ? <X /> : <RefreshCw />}
        </IconButton>
        <form className="min-w-0 flex-1" onSubmit={submit}>
          <input
            aria-label={s.address}
            value={address}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            onFocus={(e) => {
              setEditing(true);
              e.currentTarget.select();
            }}
            onBlur={() => setEditing(false)}
            onChange={(e) => setAddress(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') {
                setAddress(current);
                e.currentTarget.blur();
              }
            }}
            className="vk-focus h-7 w-full rounded-md border border-border bg-surface px-2 font-mono text-xs text-fg"
          />
        </form>
        <span className="hidden shrink-0 truncate px-1 text-xs text-muted sm:inline" title={s.environment(machine)}>
          → {machine}
        </span>
        <IconButton label={s.devtools} disabled={!view} onClick={() => view && api.devtools(view)}>
          <Bug />
        </IconButton>
        {external && (
          <IconButton label={s.openBrowser} onClick={() => app.platform.openExternal(current)}>
            <ExternalLink />
          </IconButton>
        )}
      </div>
      <div ref={box} aria-label={state?.title || s.page} className="relative min-h-0 flex-1 bg-white">
        {covered && shot && <img src={shot} alt="" className="pointer-events-none absolute inset-0 size-full object-cover object-left-top" />}
        {(openError || hostOffline || crashed) && (
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-3 bg-surface p-6">
            <Globe className="size-6 text-muted" />
            <Notice tone={openError ? 'danger' : 'warn'} className="max-w-md">
              {openError ? `${s.openFailed} ${openError}` : hostOffline ? s.hostOffline : s.crashed}
            </Notice>
          </div>
        )}
      </div>
    </div>
  );
}
