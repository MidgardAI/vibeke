import { useRef, useState, type PointerEvent as ReactPointerEvent } from 'react';
import { Bot, Crosshair, Pencil, Pin, PinOff, SquareTerminal, Trash2 } from 'lucide-react';
import { useApp, useHost, useNow } from '../app/hooks';
import { t } from '../i18n';
import { shortDuration, shortPath } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { rowTitle, type PaneRow } from '../lib/tree';
import { navigate } from '../router';
import { Button, Dot, Sheet, SheetRow, TextField, cx } from './ui';

export function useLongPress(onLong: () => void, ms = 450) {
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const fired = useRef(false);
  const origin = useRef<{ x: number; y: number } | null>(null);
  const clear = () => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
  };
  return {
    fired,
    handlers: {
      onPointerDown: (e: ReactPointerEvent) => {
        fired.current = false;
        origin.current = { x: e.clientX, y: e.clientY };
        clear();
        timer.current = setTimeout(() => {
          fired.current = true;
          onLong();
        }, ms);
      },
      onPointerMove: (e: ReactPointerEvent) => {
        const o = origin.current;
        if (o && Math.hypot(e.clientX - o.x, e.clientY - o.y) > 10) clear();
      },
      onPointerUp: clear,
      onPointerCancel: clear,
      onPointerLeave: clear,
      onContextMenu: (e: { preventDefault(): void }) => {
        e.preventDefault();
        clear();
        fired.current = true;
        onLong();
      },
    },
  };
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

export function PaneRowView({ row, showHost }: { row: PaneRow; showHost?: boolean }) {
  const [menu, setMenu] = useState(false);
  const lp = useLongPress(() => setMenu(true));
  const now = useNow(30_000);
  const r = row;
  const sub = [
    r.run ? harnessLabel(r.run.harness) : (r.pane.fg_cmdline[0]?.split('/').pop() ?? null),
    showHost ? r.hostName : null,
    shortPath(r.run?.cwd ?? r.pane.cwd, 2),
  ].filter(Boolean);
  const since = r.run ? now - r.run.execution.since_ms : null;
  return (
    <>
      <button
        type="button"
        {...lp.handlers}
        onClick={() => {
          if (lp.fired.current) return;
          navigate({ name: 'pane', host: r.host, pane: r.pane.id, view: 'term' });
        }}
        className={cx(
          'flex w-full items-center gap-3 border border-transparent px-4 py-2.5 text-left active:bg-surface-2',
          r.needsYou && 'bg-need',
        )}
        data-needs-you={r.needsYou || undefined}
        data-nav-item={r.key}
        id={`row-${r.key}`}
      >
        <span className="text-muted">{r.run ? <Bot className="size-4.5" /> : <SquareTerminal className="size-4.5" />}</span>
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-1.5">
            {r.pinned && <Pin className="size-3 shrink-0 text-faint" />}
            <span className="truncate text-base font-medium">{rowTitle(r)}</span>
          </span>
          <span className="block truncate text-xs text-muted">{sub.join(' · ')}</span>
          {r.run?.last_message && r.attention !== 'working' && <span className="mt-0.5 block truncate text-xs text-faint">{r.run.last_message}</span>}
        </span>
        <span className="flex shrink-0 flex-col items-end gap-1">
          <span className="flex items-center gap-1.5 text-xs text-muted">
            <Dot tone={stateTone(r)} />
            {stateWord(r)}
          </span>
          {r.open.length > 0 ? (
            <span className="rounded-full bg-need-strong px-1.5 text-2xs font-semibold text-black">{r.open.length}</span>
          ) : (
            since !== null && <span className="text-2xs tabular-nums text-faint">{shortDuration(since)}</span>
          )}
        </span>
      </button>
      <PaneMenu row={r} open={menu} onClose={() => setMenu(false)} />
    </>
  );
}

export function PaneMenu({ row, open, onClose }: { row: PaneRow; open: boolean; onClose(): void }) {
  const app = useApp();
  const host = useHost(row.host);
  const full = (host?.info?.scope ?? host?.record.scope) === 'full' && host?.status === 'online';
  const [renaming, setRenaming] = useState(false);
  const [name, setName] = useState(row.pane.title ?? '');
  const [armed, setArmed] = useState(false);
  const conn = app.conn(row.host);

  const act = async (f: () => Promise<unknown>) => {
    try {
      await f();
      app.haptic('success');
    } catch (e) {
      app.toast((e as Error).message, 'error');
    }
    onClose();
  };

  const close = () => {
    setRenaming(false);
    setArmed(false);
    onClose();
  };

  return (
    <Sheet open={open} onClose={close} title={rowTitle(row)}>
      {renaming ? (
        <form
          className="space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            setRenaming(false);
            void act(() => conn!.request('pane.rename', { pane: row.pane.id, title: name.trim() || null }));
          }}
        >
          <TextField label={t.panes.renamePrompt} value={name} autoFocus onChange={(e) => setName(e.target.value)} />
          <Button variant="primary" block type="submit">
            {t.save}
          </Button>
        </form>
      ) : (
        <div className="space-y-0.5">
          <SheetRow icon={row.pinned ? <PinOff className="size-5" /> : <Pin className="size-5" />} onClick={() => (app.prefs.togglePin(row.key), close())}>
            {row.pinned ? t.panes.unpin : t.panes.pin}
          </SheetRow>
          <SheetRow icon={<Pencil className="size-5" />} disabled={!full} onClick={() => setRenaming(true)}>
            {t.panes.rename}
          </SheetRow>
          <SheetRow icon={<Crosshair className="size-5" />} disabled={!full} onClick={() => void act(() => conn!.request('pane.focus', { pane: row.pane.id }))}>
            {t.panes.focusTerminal}
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
              void act(() => conn!.request('pane.close', { pane: row.pane.id }));
            }}
          >
            {armed ? t.panes.closeConfirm : t.panes.close}
          </SheetRow>
        </div>
      )}
    </Sheet>
  );
}
