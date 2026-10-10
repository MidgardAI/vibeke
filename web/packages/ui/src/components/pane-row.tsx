import { useEffect, useRef, useState, type ComponentProps, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';
import { Bot, Pencil, Pin, PinOff, SquareTerminal, Trash2 } from 'lucide-react';
import { useApp, useHost, useNow } from '../app/hooks';
import { t } from '../i18n';
import { shortDuration, shortPath } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { rowTitle, type PaneRow } from '../lib/tree';
import { navigate } from '../router';
import { Button, Dot, Row, Sheet, SheetRow, TextField, cx } from './ui';

/** A long press moves the finger less than this. */
const PRESS_SLOP = 10;
/** The hold visual starts after this (a plain tap never flashes). */
const HOLD_VISUAL_MS = 160;

/**
 * Long-press and right-click on a row. A touch held for `ms` (default 450) calls `onLong` once;
 * moving more than `PRESS_SLOP` px or a scroll cancels it. `pressing` turns true shortly after the
 * finger lands, for the hold visual. `onStart` runs on every pointer-down (prefetch). The click
 * that follows a long press must be dropped: `fired` is true until the next pointer-down.
 */
export function useLongPress(onLong: () => void, ms = 450, opts: { onStart?(): void; onFire?(): void } = {}) {
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const visual = useRef<ReturnType<typeof setTimeout> | null>(null);
  const fired = useRef(false);
  const firedAt = useRef(0);
  const touch = useRef(false);
  const origin = useRef<{ x: number; y: number } | null>(null);
  const [pressing, setPressing] = useState(false);
  const latest = useRef({ onLong, ...opts });
  latest.current = { onLong, ...opts };
  const clear = () => {
    if (timer.current) clearTimeout(timer.current);
    if (visual.current) clearTimeout(visual.current);
    timer.current = visual.current = null;
    origin.current = null;
    setPressing(false);
  };
  useEffect(() => clear, []);
  const fire = () => {
    fired.current = true;
    firedAt.current = Date.now();
    latest.current.onFire?.();
    latest.current.onLong();
  };
  return {
    fired,
    pressing,
    handlers: {
      onPointerDown: (e: ReactPointerEvent) => {
        fired.current = false;
        latest.current.onStart?.();
        // A mouse opens the actions with the right button; holding the left one is a drag.
        touch.current = e.pointerType !== 'mouse';
        if (!touch.current || (e.pointerType === 'touch' && !e.isPrimary)) return;
        clear();
        origin.current = { x: e.clientX, y: e.clientY };
        visual.current = setTimeout(() => setPressing(true), HOLD_VISUAL_MS);
        timer.current = setTimeout(() => {
          clear();
          fire();
        }, ms);
      },
      onPointerMove: (e: ReactPointerEvent) => {
        const o = origin.current;
        if (o && Math.hypot(e.clientX - o.x, e.clientY - o.y) > PRESS_SLOP) clear();
      },
      onPointerUp: clear,
      onPointerCancel: clear,
      onPointerLeave: clear,
      onContextMenu: (e: { preventDefault(): void }) => {
        e.preventDefault();
        clear();
        // Android fires `contextmenu` after a long press too: the timer may have handled it.
        if (Date.now() - firedAt.current < 800) return;
        if (touch.current) fired.current = true;
        firedAt.current = Date.now();
        latest.current.onLong();
      },
      // Keyboard: the context-menu key, and Shift+F10.
      onKeyDown: (e: ReactKeyboardEvent) => {
        if (e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10')) {
          e.preventDefault();
          e.stopPropagation();
          latest.current.onLong();
        }
      },
    },
  };
}

/**
 * A list row with the shared press behaviour: long press or right-click opens the row's actions
 * (`onActions`), a finger landing warms the destination (`onWarm`), and the click after a long
 * press is dropped. It is a `Row`, so keyboard and styling stay the same.
 */
export function PressRow({ onActions, onWarm, onClick, className, ...rest }: ComponentProps<typeof Row> & { onActions(): void; onWarm?(): void }) {
  const app = useApp();
  const press = useLongPress(onActions, 450, { onStart: onWarm, onFire: () => app.haptic('tap') });
  return (
    <Row
      {...rest}
      {...press.handlers}
      data-pressing={press.pressing || undefined}
      className={cx('vk-press', className)}
      onClick={(e) => {
        if (press.fired.current) {
          press.fired.current = false;
          e.preventDefault();
          return;
        }
        onClick?.(e);
      }}
    />
  );
}

export function stateTone(r: PaneRow): 'need' | 'ok' | 'accent' | 'muted' | 'danger' {
  if (r.attention === 'interaction') return 'need';
  if (r.attention === 'needs_input') return r.run?.execution.value === 'error' || r.run?.execution.value === 'rate_limited' ? 'danger' : 'need';
  if (r.attention === 'working') return 'accent';
  return 'muted';
}

export function stateWord(r: PaneRow): string {
  if (r.attention === 'interaction') return t.panes.state.needs_you!;
  if (r.attention === 'needs_input' && r.run?.execution.value === 'idle') return t.panes.state.finished!;
  if (r.run) return t.panes.state[r.run.execution.value] ?? r.run.execution.value;
  return r.pane.exited ? t.panes.exited : t.panes.shell;
}

/**
 * The one action sheet of a row: Pin or Unpin, Rename, Close (asks twice). Rename and Close act
 * on `target`, a pane; a workspace with several panes passes null and only offers the pin.
 */
export function RowActionSheet({
  title,
  pinned,
  onTogglePin,
  target,
  workspaceLevel,
  onClose,
}: {
  title: string;
  pinned: boolean;
  onTogglePin(): void;
  target: PaneRow | null;
  /** The sheet belongs to a workspace row: Rename and Close say that they act on its pane. */
  workspaceLevel?: boolean;
  onClose(): void;
}) {
  const app = useApp();
  const host = useHost(target?.host ?? '');
  const full = !!target && (host?.info?.scope ?? host?.record.scope) === 'full' && host?.status === 'online';
  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState(target?.pane.title ?? '');
  const [armed, setArmed] = useState(false);
  const conn = target ? app.conn(target.host) : undefined;

  const act = async (f: () => Promise<unknown>) => {
    try {
      await f();
      app.haptic('success');
    } catch (e) {
      app.toast((e as Error).message, 'error');
    }
    onClose();
  };

  return (
    <Sheet open onClose={onClose} title={title}>
      {renaming && target ? (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            setRenaming(false);
            void act(() => conn!.request('pane.rename', { pane: target.pane.id, title: name.trim() || null }));
          }}
        >
          <TextField label={t.panes.renamePrompt} value={name} data-autofocus onChange={(e) => setName(e.target.value)} />
          <Button variant="primary" block type="submit">
            {t.save}
          </Button>
        </form>
      ) : (
        <div className="space-y-0.5">
          <SheetRow icon={pinned ? <PinOff className="size-5" /> : <Pin className="size-5" />} onClick={() => (onTogglePin(), onClose())}>
            {pinned ? t.panes.unpin : t.panes.pin}
          </SheetRow>
          {target && (
            <>
              <SheetRow icon={<Pencil className="size-5" />} disabled={!full} onClick={() => setRenaming(true)}>
                {workspaceLevel ? t.panes.renamePane : t.panes.rename}
              </SheetRow>
              <SheetRow
                icon={<Trash2 className="size-5" />}
                tone="danger"
                disabled={!full}
                onClick={() => {
                  if (!armed) {
                    setArmed(true);
                    app.haptic('warning');
                    return;
                  }
                  void act(() => conn!.request('pane.close', { pane: target.pane.id }));
                }}
              >
                {armed ? t.panes.closeConfirm : t.panes.close}
              </SheetRow>
            </>
          )}
        </div>
      )}
    </Sheet>
  );
}

export function PaneMenu({ row, open, onClose }: { row: PaneRow; open: boolean; onClose(): void }) {
  const app = useApp();
  if (!open) return null;
  return <RowActionSheet title={rowTitle(row)} pinned={row.pinned} onTogglePin={() => app.prefs.togglePin(row.key)} target={row} onClose={onClose} />;
}
