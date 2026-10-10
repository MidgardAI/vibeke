// Watch an agent's browser session live on the phone: attach a screencast, poll frames while the
// app is visible, draw the newest one scaled to fit. A full-scope device can take the session
// over: taps on the picture click at the matching page position, a field types text, buttons press
// keys, and an address field navigates. Everything stops (and the take-over ends) when the view
// goes away or the app is hidden; the host gives control back to the agent.

import { useCallback, useEffect, useRef, useState } from 'react';
import { Hand, Square } from 'lucide-react';
import { RpcError, type BrowserSession } from '@vibeke/core';
import { useApp } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { FRAME_FPS, PRESS_KEYS, controllerOf, fitFrame, frameDelayMs, frameSrc, needsReattach, nextSeq, normalizeUrl, tapToPage, type Size } from '../lib/screencast';
import { useStore } from '../lib/store';
import { Button, Notice, Spinner, TextField, cx } from './ui';

const LIST_EVERY_MS = 5000;
const FAILS_BEFORE_ERROR = 3;

export function Screencast({ hostId, session: initial, canControl, onClose }: { hostId: string; session: BrowserSession; canControl: boolean; onClose(): void }) {
  const app = useApp();
  const visible = useStore(app.visible);
  const id = initial.session;
  const [src, setSrc] = useState<string | null>(null);
  const [viewport, setViewport] = useState<Size>(initial.viewport);
  const [human, setHuman] = useState(initial.human_control);
  const [mine, setMine] = useState(false);
  const [lost, setLost] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [busy, setBusy] = useState(false);
  const mineRef = useRef(false);
  const setControl = (v: boolean) => {
    mineRef.current = v;
    setMine(v);
  };

  // Attach, poll frames, detach. Re-runs when the app becomes visible again or "Try again" is used.
  useEffect(() => {
    const conn = app.conn(hostId);
    if (!conn || !visible) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let seq = 0;
    let fails = 0;
    const attach = async () => {
      const a = await conn.request('browser.attach_screencast', { session: id });
      if (live && a.width > 0 && a.height > 0) setViewport({ width: a.width, height: a.height });
      seq = 0;
    };
    const tick = async () => {
      try {
        const f = await conn.request('browser.screencast_frame', { session: id, after_seq: seq });
        fails = 0;
        const url = frameSrc(f as { mime?: string; data_b64: string | null });
        if (live && url) {
          setSrc(url);
          setLost(false);
        }
        seq = nextSeq(seq, f);
      } catch (e) {
        // The host drops a viewer that did not poll for 30 s: attach again.
        if (e instanceof RpcError && needsReattach(e.kind)) {
          try {
            await attach();
            fails = 0;
          } catch {
            fails++;
          }
        } else fails++;
        if (live && fails >= FAILS_BEFORE_ERROR) {
          setLost(true);
          return;
        }
      }
      if (live) timer = setTimeout(() => void tick(), frameDelayMs(FRAME_FPS));
    };
    void attach()
      .then(() => (live ? tick() : undefined))
      .catch(() => {
        if (live) setLost(true);
      });
    return () => {
      live = false;
      if (timer) clearTimeout(timer);
      // Hiding or leaving stops watching: give control back first, then detach.
      const wasMine = mineRef.current;
      setControl(false);
      if (wasMine) void conn.request('browser.release', { session: id }).catch(() => {});
      void conn.request('browser.detach_screencast', { session: id }).catch(() => {});
    };
  }, [app, hostId, id, visible, attempt]);

  // Who controls the page (another device may take it over, the agent may get it back).
  useEffect(() => {
    if (!visible) return;
    let live = true;
    const poll = async () => {
      try {
        const r = await app.conn(hostId)?.request('browser.list', {});
        const s = r?.sessions.find((x) => x.session === id);
        if (live && s) setHuman(s.human_control);
      } catch {
        // keep the last known value
      }
    };
    const timer = setInterval(() => void poll(), LIST_EVERY_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [app, hostId, id, visible]);

  const call = useCallback(
    async (fn: () => Promise<unknown>): Promise<boolean> => {
      setBusy(true);
      try {
        await fn();
        return true;
      } catch (e) {
        app.toast(errorMessage(e), 'error');
        return false;
      } finally {
        setBusy(false);
      }
    },
    [app],
  );
  const conn = () => app.conn(hostId);

  const takeOver = () =>
    call(async () => {
      await conn()!.request('browser.take_over', { session: id });
      setControl(true);
      setHuman(true);
    });
  const release = () =>
    call(async () => {
      await conn()!.request('browser.release', { session: id });
      setControl(false);
      setHuman(false);
    });

  const who = controllerOf(human, mine);

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-3 pb-4 pt-2">
      <div className="flex flex-wrap items-center gap-2">
        <div className="min-w-0 flex-1 truncate font-mono text-xs text-muted" title={initial.url}>
          {initial.url}
        </div>
        <span role="status" className={cx('text-xs', who === 'you' ? 'font-medium text-ok' : 'text-muted')}>
          {t.live.controller[who]}
        </span>
      </div>

      <Frame src={src} viewport={viewport} lost={lost} onRetry={() => (setLost(false), setAttempt((n) => n + 1))} onTap={mine ? (p) => void call(() => conn()!.request('browser.click', { session: id, x: p.x, y: p.y })) : undefined} />
      {mine && <div className="text-center text-xs text-muted">{t.live.tapHint}</div>}

      <div className="flex flex-wrap gap-2">
        {mine ? (
          <Button size="lg" variant="outline" icon={<Hand />} busy={busy} onClick={() => void release()}>
            {t.live.release}
          </Button>
        ) : canControl ? (
          <Button size="lg" variant="primary" icon={<Hand />} busy={busy} disabled={lost || !src} onClick={() => void takeOver()}>
            {t.live.takeOver}
          </Button>
        ) : (
          <Notice className="flex-1">{t.live.needsFull}</Notice>
        )}
        <Button size="lg" variant="ghost" icon={<Square />} onClick={onClose}>
          {t.live.stop}
        </Button>
      </div>

      {mine && <Controls session={id} hostId={hostId} busy={busy} call={call} />}
    </div>
  );
}

/** The picture, scaled to fit its box; taps map to page coordinates through `tapToPage`. */
function Frame({ src, viewport, lost, onRetry, onTap }: { src: string | null; viewport: Size; lost: boolean; onRetry(): void; onTap?: (p: { x: number; y: number }) => void }) {
  const box = useRef<HTMLDivElement>(null);
  const [avail, setAvail] = useState<Size>({ width: 0, height: 0 });
  useEffect(() => {
    const el = box.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => setAvail({ width: el.clientWidth, height: el.clientHeight }));
    ro.observe(el);
    setAvail({ width: el.clientWidth, height: el.clientHeight });
    return () => ro.disconnect();
  }, []);
  const size = fitFrame(viewport, avail);
  return (
    <div ref={box} className="flex h-[min(60dvh,520px)] min-h-48 shrink-0 items-center justify-center overflow-hidden rounded-xl border border-border bg-surface-2">
      {lost ? (
        <div className="flex flex-col items-center gap-2 px-4 text-center text-sm text-muted">
          {t.live.lost}
          <Button size="sm" onClick={onRetry}>
            {t.live.retry}
          </Button>
        </div>
      ) : src ? (
        <img
          src={src}
          alt={t.live.frameAlt}
          draggable={false}
          width={size.width || undefined}
          height={size.height || undefined}
          style={size.width ? { width: size.width, height: size.height } : { maxWidth: '100%', maxHeight: '100%' }}
          className={cx('select-none touch-manipulation', onTap && 'cursor-pointer ring-2 ring-ok/60')}
          onClick={
            onTap
              ? (e) => {
                  const r = e.currentTarget.getBoundingClientRect();
                  const p = tapToPage({ x: e.clientX - r.left, y: e.clientY - r.top }, { width: r.width, height: r.height }, viewport);
                  if (p) onTap(p);
                }
              : undefined
          }
        />
      ) : (
        <div className="flex items-center gap-2 text-sm text-muted">
          <Spinner />
          {t.live.waiting}
        </div>
      )}
    </div>
  );
}

function Controls({ session, hostId, busy, call }: { session: string; hostId: string; busy: boolean; call(fn: () => Promise<unknown>): Promise<boolean> }) {
  const app = useApp();
  const [text, setText] = useState('');
  const [address, setAddress] = useState('');
  const conn = () => app.conn(hostId)!;
  const type = async (submit: boolean) => {
    if (!text) return;
    if (await call(() => conn().request('browser.type', { session, text, submit }))) setText('');
  };
  const go = async () => {
    const url = normalizeUrl(address);
    if (url && (await call(() => conn().request('browser.navigate', { session, url })))) setAddress('');
  };
  return (
    <div className="space-y-3">
      <form
        className="flex items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          void go();
        }}
      >
        <div className="min-w-0 flex-1">
          <TextField label={t.live.address} value={address} onChange={(e) => setAddress(e.target.value)} inputMode="url" autoCapitalize="none" autoCorrect="off" spellCheck={false} placeholder="https://" className="text-base" />
        </div>
        <Button size="lg" type="submit" disabled={busy || !address.trim()}>
          {t.live.go}
        </Button>
      </form>
      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault();
          void type(false);
        }}
      >
        <TextField label={t.live.type} value={text} onChange={(e) => setText(e.target.value)} autoCapitalize="none" autoCorrect="off" spellCheck={false} placeholder={t.live.typePlaceholder} className="text-base" />
        <div className="flex gap-2">
          <Button size="lg" type="submit" className="flex-1" disabled={busy || !text}>
            {t.live.type}
          </Button>
          <Button size="lg" type="button" variant="outline" className="flex-1" disabled={busy || !text} onClick={() => void type(true)}>
            {t.live.typeSubmit}
          </Button>
        </div>
      </form>
      <div>
        <div className="mb-1 text-xs text-muted">{t.live.keys}</div>
        <div className="flex flex-wrap gap-2">
          {PRESS_KEYS.map((k) => (
            <Button key={k.key} size="md" variant="outline" className="pointer-coarse:min-w-11" disabled={busy} onClick={() => void call(() => conn().request('browser.press', { session, key: k.key }))}>
              {k.label}
            </Button>
          ))}
        </div>
      </div>
    </div>
  );
}
