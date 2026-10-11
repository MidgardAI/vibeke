// Screen mirror polling: `pane.read` every 1.5 s while working, 4 s otherwise, 300 ms ×5 after a
// send; paused when hidden, locked or offline. The last screen is cached (memory + shell cache)
// so an offline pane shows its last mirror with "last seen".
//
// Colour: `source: "styled"` (styled rows → spans); servers without it answer with an error and
// the mirror falls back to plain text for the rest of the session on this pane.
//
// Cost: each poll first reads one line to learn the pane's `revision` (it grows with every
// screen change). Only a new revision fetches the full 400 styled lines; an idle pane costs one
// tiny read per poll. Hosts that send no revision get the full read every time.

import { useCallback, useEffect, useRef, useState } from 'react';
import { RpcError, type PaneText, type StyledScreen } from '@vibeke/core';
import { useApp, useHost, useVisible } from '../../app/hooks';
import type { Segment } from '../../lib/ansi';
import { useStore } from '../../lib/store';
import { BURST_COUNT, nextPollDelay } from '../../lib/poll';
import { styledLines } from '../../lib/styled';

export interface Mirror {
  /** Plain text (styled source) or text with ANSI escapes (plain source). */
  text: string;
  /** Styled lines when the host supports `styled`; null = render `text`. */
  lines: Segment[][] | null;
  at: number | null;
  live: boolean;
  error: string | null;
}

const LINES = 400;

export function useMirror(hostId: string, pane: string, working: boolean): Mirror & { burst(): void; refresh(): void } {
  const app = useApp();
  const host = useHost(hostId);
  const visible = useVisible();
  const locked = useStore(app.locked);
  const online = host?.status === 'online';
  const key = `${hostId}/${pane}`;
  const cached = app.mirrors.get(key);
  const [m, setM] = useState<Mirror>({ text: cached?.text ?? '', lines: cached?.lines ?? null, at: cached?.at ?? null, live: false, error: null });
  const burstLeft = useRef(0);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const inflight = useRef(false);
  const lastRev = useRef<number | string | null>(null);
  /** Pane revision of the last full read (from the cheap probe before it). */
  const shownRev = useRef<number | null>(null);
  const styled = useRef(true);
  const liveRef = useRef(false);
  const [kick, setKick] = useState(0);

  // Load the persisted mirror if memory has none (cold start offline).
  useEffect(() => {
    if (cached || !app.platform.mirrorCache) return;
    let live = true;
    void app.platform.mirrorCache.get(key).then((v) => {
      if (live && v) setM((cur) => (cur.text ? cur : { text: v.text, lines: v.lines ?? null, at: v.at, live: false, error: null }));
    });
    return () => {
      live = false;
    };
  }, [key]);

  const poll = useCallback(async () => {
    const conn = app.conn(hostId);
    if (!conn || inflight.current) return;
    inflight.current = true;
    try {
      // Cheap probe: nothing changed since the last full read → only refresh "last seen".
      let rev: number | null = null;
      try {
        const probe = (await conn.request('pane.read', { pane, source: 'visible', lines: 1 })) as PaneText;
        rev = typeof probe?.revision === 'number' ? probe.revision : null;
      } catch (e) {
        if (!(e instanceof RpcError)) throw e;
      }
      if (rev !== null && rev === shownRev.current && liveRef.current) {
        setM((cur) => ({ ...cur, at: app.platform.clock.now(), live: true, error: null }));
        return;
      }
      let snap: { text: string; lines: Segment[][] | null; rev: number | string };
      let r: StyledScreen | PaneText | null = null;
      if (styled.current) {
        try {
          r = await conn.request('pane.read', { pane, source: 'styled', lines: LINES });
          // A server that ignores `source` answers with plain text: use it as such from now on.
          if (!('rows' in r) || !Array.isArray(r.rows)) styled.current = false;
        } catch (e) {
          // Only a refusal means "older server"; a lost connection is reported as usual.
          if (!(e instanceof RpcError)) throw e;
          styled.current = false;
          r = null;
        }
      }
      if (r && 'rows' in r && Array.isArray(r.rows)) {
        const { lines, text } = styledLines(r.rows);
        // No revision on styled reads: the rows themselves tell whether anything changed.
        snap = { text, lines, rev: JSON.stringify(r.rows) };
      } else if (r && 'text' in r && typeof r.text === 'string') {
        snap = { text: r.text, lines: null, rev: r.revision };
      } else {
        const p = (await conn.request('pane.read', { pane, source: 'recent', lines: LINES })) as PaneText;
        snap = { text: p.text, lines: null, rev: p.revision };
      }
      const at = app.platform.clock.now();
      shownRev.current = rev;
      if (snap.rev !== lastRev.current || !liveRef.current) {
        liveRef.current = true;
        lastRev.current = snap.rev;
        const v = { text: snap.text, at, ...(snap.lines ? { lines: snap.lines } : {}) };
        app.mirrors.set(key, v);
        setM({ text: snap.text, lines: snap.lines, at, live: true, error: null });
        void app.platform.mirrorCache?.set(key, v).catch(() => {});
      } else {
        setM((cur) => ({ ...cur, at, live: true, error: null }));
      }
    } catch (e) {
      liveRef.current = false;
      shownRev.current = null;
      setM((cur) => ({ ...cur, live: false, error: (e as Error).message }));
    } finally {
      inflight.current = false;
    }
  }, [app, hostId, pane, key]);

  useEffect(() => {
    if (timer.current) clearTimeout(timer.current);
    const delay = nextPollDelay({ visible, locked, online, working, burst: burstLeft.current });
    if (delay === null) {
      if (!online) {
        liveRef.current = false;
        setM((cur) => (cur.live ? { ...cur, live: false } : cur));
      }
      return;
    }
    timer.current = setTimeout(async () => {
      if (burstLeft.current > 0) burstLeft.current--;
      await poll();
      setKick((k) => k + 1);
    }, kick === 0 ? 0 : delay);
    return () => {
      if (timer.current) clearTimeout(timer.current);
    };
  }, [visible, locked, online, working, kick, poll]);

  return {
    ...m,
    burst: () => {
      burstLeft.current = BURST_COUNT;
      setKick((k) => k + 1);
    },
    refresh: () => void poll(),
  };
}
