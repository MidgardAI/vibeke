// Prompt-cache chip: a small countdown on idle agent runs (sidebar rows, workspace title). Green
// while the cache is warm, red in the last quarter, then "cold". Tapping it explains the idea.

import { useState, type MouseEvent } from 'react';
import { Timer } from 'lucide-react';
import type { AgentRun } from '@vibeke/core';
import { useApp, usePrefs } from '../app/hooks';
import { t } from '../i18n';
import { cacheLabel, createSharedClock, runCacheStatus, type CacheStatus } from '../lib/cache-clock';
import { whenText } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { useStore, type Readable } from '../lib/store';
import { navigate } from '../router';
import { Button, Sheet, cx } from './ui';

const clocks = new WeakMap<object, Readable<number>>();

/** The one clock every chip reads: it ticks only while a chip is mounted and the page is visible. */
function useCacheNow(): number {
  const app = useApp();
  let clock = clocks.get(app);
  if (!clock) {
    clock = createSharedClock({
      now: () => app.platform.clock.now(),
      visible: app.visible,
      setInterval: (f, ms) => setInterval(f, ms),
      clearInterval: (h) => clearInterval(h as ReturnType<typeof setInterval>),
    });
    clocks.set(app, clock);
  }
  return useStore(clock);
}

const TONE: Record<CacheStatus['state'], string> = {
  warm: 'bg-add/10 text-add',
  low: 'bg-danger/10 text-danger',
  cold: 'bg-surface-2 text-faint',
};

/**
 * `inRow`: the chip sits inside a list row that is itself a button, so it is a presentation-only
 * span there (tap still works; it is not a separate tab stop).
 */
export function CacheChip({ run, inRow, className }: { run: AgentRun | null | undefined; inRow?: boolean; className?: string }) {
  const prefs = usePrefs();
  const now = useCacheNow();
  const [open, setOpen] = useState(false);
  const s = runCacheStatus(run, now, prefs.cacheTtl);
  if (!s || !run) return null;

  const label = s.state === 'cold' ? t.cache.cold : cacheLabel(s.remainingMs);
  const name = s.state === 'cold' ? t.cache.chipCold : t.cache.chip(label);
  const show = (e: MouseEvent) => {
    e.stopPropagation();
    setOpen(true);
  };
  // The sheet is a React child of the chip: its events must not reach the row underneath.
  const stop = (e: { stopPropagation(): void }) => e.stopPropagation();
  const cls = cx(
    // A 44px touch target around a 16px chip.
    'relative inline-flex h-4 shrink-0 items-center gap-0.5 rounded-full px-1.5 text-2xs font-medium leading-none tabular-nums before:absolute before:-inset-3 before:content-[""] pointer-fine:before:hidden',
    TONE[s.state],
    className,
  );
  const inner = (
    <>
      <Timer className="size-2.5" strokeWidth={2.25} />
      {label}
    </>
  );
  return (
    <span onClick={stop} onPointerDown={stop} onKeyDown={stop} onContextMenu={stop} className="inline-flex">
      {inRow ? (
        <span role="button" aria-label={name} title={name} className={cx(cls, 'cursor-pointer')} onClick={show}>
          {inner}
        </span>
      ) : (
        <button
          type="button"
          aria-label={name}
          title={name}
          className={cx(cls, 'vk-focus')}
          onClick={show}
        >
          {inner}
        </button>
      )}
      {open && <CacheSheet run={run} status={s} onClose={() => setOpen(false)} />}
    </span>
  );
}

function CacheSheet({ run, status, onClose }: { run: AgentRun; status: CacheStatus & { ttlMs: number; sinceMs: number }; onClose(): void }) {
  const app = useApp();
  const now = app.platform.clock.now();
  const label = cacheLabel(status.remainingMs);
  const state =
    status.state === 'cold' ? t.cache.coldNow(whenText(status.sinceMs + status.ttlMs, now)) : status.remainingMs < 60_000 ? t.cache.lessThanMinute : t.cache.left(label);
  return (
    <Sheet open onClose={onClose} title={t.cache.sheetTitle}>
      <div className="space-y-3 text-sm">
        <p className="text-fg/90">{t.cache.what}</p>
        <p className={cx('font-medium', status.state === 'low' && 'text-danger', status.state === 'warm' && 'text-add')}>{state}</p>
        {status.state === 'low' && <p className="text-muted">{t.cache.lowHint}</p>}
        <p className="text-muted">{t.cache.ttl(harnessLabel(run.harness), Math.round(status.ttlMs / 60_000))}</p>
        <Button
          variant="outline"
          block
          onClick={() => {
            onClose();
            navigate({ name: 'settings' });
          }}
        >
          {t.cache.change}
        </Button>
      </div>
    </Sheet>
  );
}
