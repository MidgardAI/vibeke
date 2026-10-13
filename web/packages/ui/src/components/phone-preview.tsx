// The phone's view of a preview: the host renders the page in its headless browser (a mobile
// session sized to this area) and streams it as pictures; touches and keys go back as input
// events (spec 06 B3.6). One long poll stays open while the app is visible. The session closes
// when this view goes away or the app stays hidden for a while.

import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react';
import { ArrowLeft, ArrowRight, Keyboard, Maximize2, MessageSquarePlus, Minimize2, RotateCw } from 'lucide-react';
import { RpcError, type BrowserSession } from '@vibeke/core';
import { useApp, useConnOf } from '../app/hooks';
import { t } from '../i18n';
import { LONG_PRESS_MS, dragged, pickParams, regionStyle, useAnnotations, type Pt } from '../lib/annotations';
import { errorMessage } from '../lib/answer';
import { InputBatcher, inputToEvent, namedKey, sizeChanged, toViewport, usableSize } from '../lib/phone-preview';
import { FRAME_FPS, frameDelayMs, frameSrc, needsReattach, nextSeq, normalizeUrl, type Size } from '../lib/screencast';
import { useStore } from '../lib/store';
import { AnnotationNotes } from './annotation-notes';
import { Button, IconButton, Notice, Sheet, Spinner, cx } from './ui';

const WAIT_MS = 10000;
const FAILS_BEFORE_ERROR = 3;
/** The host ends a session that nobody polls for 30 s; close ours earlier when the app stays hidden. */
const HIDDEN_CLOSE_MS = 20000;
const URL_EVERY_MS = 8000;
const RESIZE_DEBOUNCE_MS = 150;
/** A poll that returns this fast without a picture is not really waiting (an older host): pause between polls. */
const FAST_EMPTY_MS = 250;

export function PhonePreview({
  hostId,
  preview,
  onClose,
  expanded,
  onExpand,
  confined,
}: {
  hostId: string;
  preview: { id: string; machine?: string };
  /** A review's confined session (spec 20 §3.7): the address is read-only. */
  confined?: boolean;
  onClose?: () => void;
  /** Shown in the centre instead of the panel (the Expand / Collapse button). */
  expanded?: boolean;
  onExpand?: () => void;
}) {
  const app = useApp();
  const connOf = useConnOf();
  const visible = useStore(app.visible);
  const stage = useRef<HTMLDivElement>(null);
  const hidden = useRef<HTMLInputElement>(null);
  const area = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState<Size>({ width: 0, height: 0 });
  const [src, setSrc] = useState<string | null>(null);
  const [url, setUrl] = useState('');
  const [phase, setPhase] = useState<'opening' | 'ready' | 'error'>('opening');
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [editing, setEditing] = useState(false);
  const [address, setAddress] = useState('');

  const session = useRef<string | null>(null);
  // The open session as state, for the notes of the page review (the ref is for the input paths).
  const [sid, setSid] = useState<string | null>(null);
  const [annotating, setAnnotating] = useState(false);
  const [notesOpen, setNotesOpen] = useState(false);
  const [focusNote, setFocusNote] = useState<number | null>(null);
  const [drag, setDrag] = useState<{ a: Pt; b: Pt } | null>(null);
  const gesture = useRef<{ id: number; start: Pt; region: boolean; cancelled: boolean; timer: ReturnType<typeof setTimeout> | null } | null>(null);
  const notes = useAnnotations(hostId, 'session', sid, annotating);
  const viewport = useRef<Size>({ width: 0, height: 0 });
  const sizeRef = useRef<Size>({ width: 0, height: 0 });
  const batcher = useRef(new InputBatcher());
  const inFlight = useRef(false);
  const raf = useRef<number | null>(null);
  const editingRef = useRef(false);
  editingRef.current = editing;

  // ---- measuring: the area in CSS pixels, also when the keyboard or a rotation changes it ----
  useEffect(() => {
    // The unconstrained container, not the stage: the stage's height is set from this
    // measurement, so measuring it could never grow again after the keyboard closes.
    const el = area.current;
    if (!el) return;
    const measure = () => {
      const r = el.getBoundingClientRect();
      const next = usableSize(r, window.visualViewport ?? null);
      if (sizeChanged(next, sizeRef.current)) {
        sizeRef.current = next;
        setSize(next);
      }
    };
    measure();
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    ro?.observe(el);
    const vv = window.visualViewport;
    vv?.addEventListener('resize', measure);
    vv?.addEventListener('scroll', measure);
    window.addEventListener('resize', measure);
    window.addEventListener('orientationchange', measure);
    return () => {
      ro?.disconnect();
      vv?.removeEventListener('resize', measure);
      vv?.removeEventListener('scroll', measure);
      window.removeEventListener('resize', measure);
      window.removeEventListener('orientationchange', measure);
    };
  }, []);

  // ---- sending input: one batch per animation frame, never two calls at once ----
  const flush = useCallback(() => {
    raf.current = null;
    const id = session.current;
    const conn = connOf(hostId);
    if (!id || !conn || inFlight.current || !batcher.current.pending) return;
    const events = batcher.current.take();
    inFlight.current = true;
    conn
      .request('browser.input', { session: id, events })
      .catch((e) => {
        if (e instanceof RpcError && needsReattach(e.kind)) setAttempt((n) => n + 1);
        else app.toast(errorMessage(e), 'error');
      })
      .finally(() => {
        inFlight.current = false;
        if (batcher.current.pending) schedule();
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [app, hostId]);
  const schedule = useCallback(() => {
    if (raf.current !== null || inFlight.current) return;
    raf.current = requestAnimationFrame(flush);
  }, [flush]);
  const send = useCallback(
    (ev: Parameters<InputBatcher['push']>[0]) => {
      batcher.current.push(ev);
      schedule();
    },
    [schedule],
  );

  // ---- the session: open, long-poll frames, and close when we are done ----
  useEffect(() => {
    const conn = connOf(hostId);
    if (!conn || !visible || size.width < 8 || size.height < 8) return;
    let live = true;
    let seq = 0;
    let fails = 0;
    const open = async () => {
      setPhase('opening');
      const s: BrowserSession = await conn.request('browser.preview_open', {
        preview: preview.id,
        ...(preview.machine ? { machine: preview.machine } : {}),
        viewport: sizeRef.current,
        dpr: window.devicePixelRatio || 1,
        dark: window.matchMedia?.('(prefers-color-scheme: dark)').matches,
      });
      if (!live) {
        void conn.request('browser.session_close', { session: s.session }).catch(() => {});
        return;
      }
      session.current = s.session;
      setSid(s.session);
      viewport.current = s.viewport?.width > 0 ? s.viewport : sizeRef.current;
      setUrl(s.url);
      seq = 0;
      batcher.current.clear();
    };
    const run = async () => {
      try {
        if (!session.current) await open();
      } catch (e) {
        if (live) {
          setError(errorMessage(e));
          setPhase('error');
        }
        return;
      }
      while (live && session.current) {
        const started = Date.now();
        try {
          const f = await conn.request('browser.screencast_frame', { session: session.current, after_seq: seq, wait_ms: WAIT_MS });
          fails = 0;
          const img = frameSrc(f as { mime?: string; data_b64: string | null });
          if (live && img) {
            setSrc(img);
            setPhase('ready');
          } else if (live) setPhase((p) => (p === 'opening' ? 'ready' : p));
          seq = nextSeq(seq, f);
          if (!img && Date.now() - started < FAST_EMPTY_MS) await new Promise((r) => setTimeout(r, frameDelayMs(FRAME_FPS)));
        } catch (e) {
          if (!live) return;
          if (e instanceof RpcError && needsReattach(e.kind)) {
            // The host dropped our session: open a new one.
            session.current = null;
            setSid(null);
            try {
              await open();
              fails = 0;
              continue;
            } catch (e2) {
              setError(errorMessage(e2));
              setPhase('error');
              return;
            }
          }
          fails++;
          if (fails >= FAILS_BEFORE_ERROR) {
            setError(t.phone.lost);
            setPhase('error');
            return;
          }
          await new Promise((r) => setTimeout(r, 500 * fails));
        }
      }
    };
    void run();
    return () => {
      live = false;
    };
    // The session is opened once per visible period; a resize is sent as an event, not a reopen.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [app, hostId, preview.id, preview.machine, visible, attempt, size.width > 7 && size.height > 7]);

  const close = useCallback(() => {
    const id = session.current;
    session.current = null;
    setSid(null);
    batcher.current.clear();
    if (id) void connOf(hostId)?.request('browser.session_close', { session: id }).catch(() => {});
  }, [app, hostId]);
  // Hidden for a while: close the session (the next show opens a new one). Leaving: close now.
  useEffect(() => {
    if (visible) return;
    const timer = setTimeout(close, HIDDEN_CLOSE_MS);
    return () => clearTimeout(timer);
  }, [visible, close]);
  useEffect(() => close, [close]);
  useEffect(
    () => () => {
      if (raf.current !== null) cancelAnimationFrame(raf.current);
    },
    [],
  );

  // Rotation, keyboard: tell the host the new size (debounced; the picture follows).
  useEffect(() => {
    if (!session.current || size.width < 8 || size.height < 8) return;
    if (!sizeChanged(size, viewport.current)) return;
    const timer = setTimeout(() => {
      viewport.current = size;
      send({ type: 'resize', width: size.width, height: size.height, dpr: window.devicePixelRatio || 1 });
    }, RESIZE_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [size, send]);

  // The page's own address changes as links are followed: refresh it now and then.
  useEffect(() => {
    if (!visible || phase !== 'ready') return;
    let live = true;
    const poll = async () => {
      try {
        const r = await connOf(hostId)?.request('browser.list', {});
        const s = r?.sessions.find((x) => x.session === session.current);
        if (live && s && !editingRef.current) setUrl(s.url);
      } catch {
        // keep the last known address
      }
    };
    const timer = setInterval(() => void poll(), URL_EVERY_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [app, hostId, visible, phase]);

  // ---- touch ----
  const point = (e: ReactPointerEvent<HTMLElement>) => toViewport({ x: e.clientX, y: e.clientY }, e.currentTarget.getBoundingClientRect(), viewport.current.width > 0 ? viewport.current : sizeRef.current);
  const onDown = (e: ReactPointerEvent<HTMLElement>) => {
    if (e.pointerType === 'mouse' && e.button !== 0) return;
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      // not capturable (synthetic event): keep going
    }
    const p = point(e);
    batcher.current.down(e.pointerId, p.x, p.y);
    schedule();
  };
  const onMove = (e: ReactPointerEvent<HTMLElement>) => {
    const p = point(e);
    batcher.current.move(e.pointerId, p.x, p.y);
    if (batcher.current.pending) schedule();
  };
  const onUp = (e: ReactPointerEvent<HTMLElement>, cancel = false) => {
    if (!cancel) {
      const p = point(e);
      batcher.current.move(e.pointerId, p.x, p.y);
    }
    batcher.current.up(e.pointerId, cancel);
    schedule();
  };
  // ---- annotating: a tap picks the element under it; a long press (or a mouse drag) picks an area ----
  const vp = () => (viewport.current.width > 0 ? viewport.current : sizeRef.current);
  const endGesture = () => {
    const g = gesture.current;
    if (g?.timer) clearTimeout(g.timer);
    gesture.current = null;
    setDrag(null);
  };
  useEffect(() => {
    if (!annotating) endGesture();
  }, [annotating]);
  const onADown = (e: ReactPointerEvent<HTMLElement>) => {
    if (e.pointerType === 'mouse' && e.button !== 0) return;
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      // not capturable (synthetic event): keep going
    }
    endGesture();
    const start = point(e);
    const g = { id: e.pointerId, start, region: false, cancelled: false, timer: null as ReturnType<typeof setTimeout> | null };
    if (e.pointerType !== 'mouse') {
      g.timer = setTimeout(() => {
        g.region = true;
        g.timer = null;
        setDrag({ a: start, b: start });
      }, LONG_PRESS_MS);
    }
    gesture.current = g;
  };
  const onAMove = (e: ReactPointerEvent<HTMLElement>) => {
    const g = gesture.current;
    if (!g || g.id !== e.pointerId) return;
    const p = point(e);
    if (!g.region && dragged(g.start, p)) {
      if (e.pointerType === 'mouse') g.region = true;
      else {
        // Moved before the long press: this is a scroll attempt, not a pick.
        g.cancelled = true;
        if (g.timer) clearTimeout(g.timer);
        g.timer = null;
      }
    }
    if (g.region) setDrag({ a: g.start, b: p });
  };
  const onAUp = (e: ReactPointerEvent<HTMLElement>, cancel = false) => {
    const g = gesture.current;
    if (!g || g.id !== e.pointerId) return;
    const end = point(e);
    endGesture();
    const id = session.current;
    if (cancel || g.cancelled || !id) return;
    const { x, y, w, h } = pickParams(id, g.start, end, g.region);
    void notes.pick({ x, y, ...(w !== undefined ? { w, h } : {}) }).then((n) => {
      if (n) {
        setFocusNote(n.n);
        setNotesOpen(true);
      }
    });
  };
  // A wheel (mouse or trackpad in a desktop browser) scrolls the page.
  useEffect(() => {
    const el = stage.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const p = toViewport({ x: e.clientX, y: e.clientY }, el.getBoundingClientRect(), viewport.current.width > 0 ? viewport.current : sizeRef.current);
      batcher.current.wheel(p.x, p.y, e.deltaX, e.deltaY);
      schedule();
    };
    el.addEventListener('wheel', onWheel, { passive: false });
    return () => el.removeEventListener('wheel', onWheel);
  }, [schedule]);

  // ---- keyboard: a hidden input takes the focus; its text and keys become input events ----
  useEffect(() => {
    const el = hidden.current;
    if (!el) return;
    const onBefore = (e: InputEvent) => {
      if (e.isComposing && e.inputType === 'insertCompositionText') return;
      const ev = inputToEvent(e.inputType, e.data ?? el.value);
      e.preventDefault();
      if (ev) send(ev);
    };
    const onKey = (e: KeyboardEvent) => {
      const key = namedKey(e.key);
      if (!key || e.isComposing || e.ctrlKey || e.metaKey) return;
      e.preventDefault();
      send({ type: 'key', key });
    };
    const onCompEnd = (e: CompositionEvent) => {
      if (e.data) send({ type: 'text', text: e.data });
      el.value = '';
    };
    el.addEventListener('beforeinput', onBefore);
    el.addEventListener('keydown', onKey);
    el.addEventListener('compositionend', onCompEnd);
    return () => {
      el.removeEventListener('beforeinput', onBefore);
      el.removeEventListener('keydown', onKey);
      el.removeEventListener('compositionend', onCompEnd);
    };
  }, [send]);

  const navigate = async () => {
    const target = normalizeUrl(address);
    const id = session.current;
    if (!target || !id || confined) return;
    setEditing(false);
    setUrl(target);
    try {
      const r = await connOf(hostId)?.request('browser.navigate', { session: id, url: target });
      if (r?.final_url) setUrl(r.final_url);
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    }
  };

  const ready = phase === 'ready';
  const retry = () => {
    setError('');
    setAttempt((n) => n + 1);
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div role="toolbar" aria-label={t.phone.toolbar} className="flex shrink-0 items-center gap-1 border-b border-border px-2 py-1">
        <IconButton label={t.phone.back} disabled={!ready} onClick={() => send({ type: 'history', action: 'back' })}>
          <ArrowLeft />
        </IconButton>
        <IconButton label={t.phone.forward} disabled={!ready} onClick={() => send({ type: 'history', action: 'forward' })}>
          <ArrowRight />
        </IconButton>
        <IconButton label={t.phone.reload} disabled={!ready} onClick={() => send({ type: 'history', action: 'reload' })}>
          <RotateCw />
        </IconButton>
        <form
          className="min-w-0 flex-1"
          onSubmit={(e) => {
            e.preventDefault();
            void navigate();
          }}
        >
          <input
            aria-label={t.phone.address}
            value={editing ? address : url}
            onFocus={(e) => {
              setAddress(url);
              setEditing(true);
              e.currentTarget.select();
            }}
            onBlur={() => setEditing(false)}
            onChange={(e) => setAddress(e.target.value)}
            disabled={!ready}
            readOnly={confined}
            inputMode="url"
            enterKeyHint="go"
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
            className="h-8 w-full rounded-md border border-border bg-bg px-2.5 font-mono text-xs text-fg focus:border-border-strong focus:outline-none focus-visible:ring-2 focus-visible:ring-fg/15 pointer-coarse:h-10 pointer-coarse:text-base"
          />
        </form>
        <IconButton label={t.phone.keyboard} disabled={!ready} onClick={() => hidden.current?.focus()}>
          <Keyboard />
        </IconButton>
        <IconButton label={annotating ? t.phone.stopAnnotate : t.phone.annotate} aria-pressed={annotating} active={annotating} disabled={!ready} onClick={() => setAnnotating((v) => !v)}>
          <MessageSquarePlus />
        </IconButton>
        {annotating && (
          <Button size="sm" variant="outline" onClick={() => setNotesOpen(true)}>
            {t.annotate.open(notes.count)}
          </Button>
        )}
        {onExpand && (
          <IconButton label={expanded ? t.embeddedPreview.collapse : t.embeddedPreview.expand} onClick={onExpand}>
            {expanded ? <Minimize2 /> : <Maximize2 />}
          </IconButton>
        )}
        {onClose && (
          <Button size="sm" variant="ghost" onClick={onClose}>
            {t.live.stop}
          </Button>
        )}
      </div>

      <div ref={area} className="relative min-h-0 flex-1 overflow-hidden bg-surface-2">
        <div
          ref={stage}
          role="img"
          aria-label={t.phone.surface}
          className={cx('absolute inset-x-0 top-0 select-none', ready && 'touch-none')}
          style={{ height: size.height > 0 ? size.height : '100%', touchAction: 'none' }}
          onPointerDown={ready ? (annotating ? onADown : onDown) : undefined}
          onPointerMove={ready ? (annotating ? onAMove : onMove) : undefined}
          onPointerUp={ready ? (e) => (annotating ? onAUp(e) : onUp(e)) : undefined}
          onPointerCancel={ready ? (e) => (annotating ? onAUp(e, true) : onUp(e, true)) : undefined}
          onContextMenu={(e) => e.preventDefault()}
        >
          {src && <img src={src} alt={t.phone.frameAlt} draggable={false} className="pointer-events-none h-full w-full select-none object-fill" />}
          {drag && <div aria-hidden className="pointer-events-none absolute border-2 border-accent bg-accent/15" style={regionStyle(drag, vp())} />}
        </div>
        {annotating && ready && (
          <div role="status" className="pointer-events-none absolute inset-x-3 bottom-3 rounded-md bg-surface/90 px-2 py-1 text-center text-xs text-muted">
            {t.annotate.tapHint}
          </div>
        )}
        <input
          ref={hidden}
          aria-label={t.phone.typeHere}
          type="text"
          autoCapitalize="none"
          autoCorrect="off"
          autoComplete="off"
          spellCheck={false}
          tabIndex={-1}
          className="absolute bottom-0 left-0 size-px opacity-0"
        />
        {phase === 'opening' && (
          <div role="status" className="pointer-events-none absolute inset-0 flex items-center justify-center gap-2 text-sm text-muted">
            <Spinner />
            {t.phone.opening}
          </div>
        )}
        {phase === 'error' && (
          <div className="absolute inset-x-3 top-3">
            <Notice
              tone="danger"
              action={
                <Button size="sm" onClick={retry}>
                  {t.phone.retry}
                </Button>
              }
            >
              {error || t.phone.openFailed}
            </Notice>
          </div>
        )}
      </div>
      <Sheet open={notesOpen} onClose={() => (setNotesOpen(false), setFocusNote(null))} title={t.annotate.title}>
        <AnnotationNotes
          heading={false}
          focus={focusNote}
          draft={notes.draft}
          busy={notes.busy}
          onEdit={(n, text) => void notes.edit(n, text)}
          onPageNote={(text) => void notes.setPageNote(text)}
          onRemove={(n) => void notes.remove(n)}
          onSend={() => void notes.send().then(() => setNotesOpen(false))}
          onSendNow={() => void notes.sendNow()}
          onCopy={() => void notes.copy()}
          onClear={() => void notes.clear()}
        />
      </Sheet>
    </div>
  );
}
