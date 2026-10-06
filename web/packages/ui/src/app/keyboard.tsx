// Keyboard layer (spec 16 §16.2): global shortcuts, the command palette, the `?` cheat sheet,
// and shell commands from native menus. Mounted once per window by <VibekeApp/>.

import { useEffect, useMemo, useState } from 'react';
import { CommandPalette, useEntityItems, type PaletteItem } from '../components/command-palette';
import { dialogOpen } from '../components/dialog';
import { NewSheet } from '../components/new-sheet';
import { Sheet } from '../components/ui';
import { t } from '../i18n';
import { listNav, type NavAct } from '../lib/list-nav';
import { SHORTCUTS, keyLabel, shortcutFor, type ShortcutAction } from '../lib/shortcuts';
import type { PaneRow } from '../lib/tree';
import type { UiCommand } from '../platform';
import { formatRoute, goBack, navigate, useRoute, type Route } from '../router';
import { HandoffSheet } from '../screens/handoff';
import { ShareSheet } from '../screens/share';
import { useApp, usePrefs, useTree } from './hooks';

export type Surface = 'full' | 'quick' | 'pane';

export function isMacLike(explicit?: boolean): boolean {
  if (explicit !== undefined) return explicit;
  if (typeof navigator === 'undefined') return false;
  return /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);
}

/** In-app command bus (sidebar buttons, etc.), same commands as the shell's `onCommand`. */
const bus = new Set<(cmd: UiCommand) => void>();
export function emitUi(cmd: UiCommand): void {
  for (const f of [...bus]) f(cmd);
}

type SheetState = { kind: 'new' } | { kind: 'share'; row: PaneRow } | { kind: 'handoff'; row: PaneRow } | null;

const TYPING = /^(INPUT|TEXTAREA|SELECT)$/;
const SUBPAGES = new Set<Route['name']>(['pane', 'interaction', 'run', 'settings', 'crew', 'pair', 'not_found']);

/** Press the `[data-find]` control of the current screen, or focus its `[data-find-input]`. */
function find(): boolean {
  const input = document.querySelector<HTMLInputElement>('[data-find-input]');
  if (input && input.getClientRects().length) {
    input.focus();
    input.select();
    return true;
  }
  const btn = document.querySelector<HTMLElement>('[data-find]');
  if (btn && btn.getClientRects().length) {
    btn.click();
    return true;
  }
  return false;
}

export function KeyboardLayer({ surface }: { surface: Surface }) {
  const app = useApp();
  const route = useRoute();
  const prefs = usePrefs();
  const tree = useTree();
  const mac = isMacLike(app.platform.mac);
  const [palette, setPalette] = useState(false);
  const [help, setHelp] = useState(false);
  const [sheet, setSheet] = useState<SheetState>(null);
  const routeKey = formatRoute(route);

  useEffect(() => listNav.clear(), [routeKey]);
  useEffect(() => listNav.attach(), []);

  const paneRow = route.name === 'pane' ? tree.all.find((r) => r.host === route.host && r.pane.id === route.pane) : undefined;

  useEffect(() => {
    const act = (a: NavAct) => {
      const r = listNav.act(a);
      if (r === 'unavailable') app.toast(t.keys.unavailable!, 'warn', 1800);
      return r !== 'none';
    };
    const run = (a: ShortcutAction): boolean => {
      switch (a.type) {
        case 'palette':
          setPalette((v) => !v);
          return true;
        case 'help':
          setHelp(true);
          return true;
        case 'tab':
          if (surface !== 'full') return false;
          navigate({ name: a.tab });
          return true;
        case 'next':
          return listNav.move(1);
        case 'prev':
          return listNav.move(-1);
        case 'allow':
          return act('allow');
        case 'deny':
          return act('deny');
        case 'allowAlways':
          return act('allow_always');
        case 'open':
          return act('open');
        case 'find':
          if (find()) return true;
          if (surface === 'full') setPalette(true);
          return surface === 'full';
        case 'back':
          if (surface === 'quick') {
            app.platform.windows?.close?.();
            return true;
          }
          if (surface === 'full' && SUBPAGES.has(route.name)) {
            goBack({ name: 'home' });
            return true;
          }
          return false;
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      const target = e.target instanceof HTMLElement ? e.target : null;
      const typing = !!target && (target.isContentEditable || TYPING.test(target.tagName));
      const dialog = dialogOpen();
      if (typing && e.key === 'Escape' && !dialog) {
        target!.blur();
        return;
      }
      const onControl = !!target && (target.tagName === 'BUTTON' || target.tagName === 'A');
      const a = shortcutFor(e, { mac, typing, dialog, onControl });
      if (!a) return;
      // The palette toggles itself even over other dialogs; everything else waits for them.
      if (dialog && a.type !== 'palette') return;
      if (run(a)) e.preventDefault();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [app, mac, surface, route.name]);

  // Commands from native menus / tray, and from in-app buttons.
  useEffect(() => {
    const handle = (cmd: UiCommand) => {
        switch (cmd) {
          case 'palette':
            return setPalette(true);
          case 'shortcuts':
            return setHelp(true);
          case 'find':
            if (!find()) setPalette(true);
            return;
          case 'new-agent':
            return setSheet({ kind: 'new' });
          case 'inbox':
          case 'panes':
          case 'focus':
          case 'changes':
          case 'settings':
            return navigate({ name: cmd });
          case 'pair':
            return navigate({ name: 'pair', d: null });
          case 'back':
            return goBack({ name: 'home' });
          case 'pop-out':
            if (route.name === 'pane') app.platform.windows?.popOutPane?.(route.host, route.pane);
            return;
        }
    };
    bus.add(handle);
    const off = app.platform.onCommand?.(handle);
    return () => {
      bus.delete(handle);
      off?.();
    };
  }, [app, routeKey]);

  const entities = useEntityItems({
    pane: (host, pane) => navigate({ name: 'pane', host, pane, view: 'term' }),
    workspace: (host, ws) => {
      navigate({ name: 'panes' });
      // Scroll to the workspace's first row once the list rendered.
      setTimeout(() => {
        const row = tree.all.find((r) => r.host === host && r.workspace?.id === ws);
        if (row) document.getElementById(`row-${row.key}`)?.scrollIntoView({ block: 'start', behavior: 'smooth' });
      }, 50);
    },
    host: () => navigate({ name: 'crew' }),
    interaction: (host, id) => navigate({ name: 'interaction', host, id, preselect: null }),
  });

  const commands = useMemo((): PaletteItem[] => {
    const mod = (k: string) => keyLabel(mac, `mod+${k}`);
    const c = (id: string, title: string, run: () => void, shortcut?: string, keywords?: string): PaletteItem => ({ id: `c:${id}`, group: 'command', title, run, shortcut, keywords });
    const out: PaletteItem[] = [
      c('inbox', `${t.palette.go} ${t.tabs.inbox}`, () => navigate({ name: 'inbox' }), mod('1')),
      c('panes', `${t.palette.go} ${t.tabs.panes}`, () => navigate({ name: 'panes' }), mod('2')),
      c('focus', `${t.palette.go} ${t.tabs.focus}`, () => navigate({ name: 'focus' }), mod('3')),
      c('changes', `${t.palette.go} ${t.tabs.changes}`, () => navigate({ name: 'changes' }), mod('4')),
      c('new', t.palette.newAgent, () => setSheet({ kind: 'new' }), undefined, 'start agent tab'),
      c('pair', t.palette.pair, () => navigate({ name: 'pair', d: null }), undefined, 'connect link qr'),
      c('settings', t.palette.settings, () => navigate({ name: 'settings' }), mac ? '⌘,' : undefined, 'preferences'),
      c('shortcuts', t.palette.shortcuts, () => setHelp(true), '?', 'keys help'),
      c('theme', t.palette.theme(prefs.theme === 'dark' ? 'light' : 'dark'), () => app.prefs.patch({ theme: prefs.theme === 'dark' ? 'light' : 'dark' }), undefined, 'dark light appearance'),
    ];
    // Lock pauses polling until "Resume": only the main window has that overlay.
    if (surface === 'full') out.push(c('lock', t.palette.lock, () => app.locked.set(true)));
    if (paneRow) {
      const full = paneRow && (app.conn(paneRow.host)?.getSnapshot().info?.scope ?? 'view') === 'full';
      if (full) {
        out.unshift(c('handoff', t.palette.handoff, () => setSheet({ kind: 'handoff', row: paneRow }), undefined, 'move'));
        out.unshift(c('share', t.palette.share, () => setSheet({ kind: 'share', row: paneRow }), undefined, 'invite'));
      }
      const pop = app.platform.windows?.popOutPane;
      if (pop && surface === 'full') out.unshift(c('popout', t.palette.popOut, () => pop(paneRow.host, paneRow.pane.id), mac ? '⇧⌘O' : 'Ctrl+Shift+O', 'window'));
    }
    for (const x of app.platform.extensions?.commands?.() ?? []) out.push({ id: `x:${x.id}`, group: 'command', title: x.title, sub: x.hint, keywords: x.keywords, run: x.run });
    return out;
  }, [mac, prefs.theme, paneRow, app, surface]);

  if (surface === 'quick') {
    return <CheatSheet open={help} onClose={() => setHelp(false)} mac={mac} />;
  }
  return (
    <>
      <CommandPalette open={palette} onClose={() => setPalette(false)} items={surface === 'full' ? [...commands, ...entities] : commands} />
      <CheatSheet open={help} onClose={() => setHelp(false)} mac={mac} />
      <NewSheet open={sheet?.kind === 'new'} onClose={() => setSheet(null)} />
      {sheet?.kind === 'share' && <ShareSheet row={sheet.row} open onClose={() => setSheet(null)} />}
      {sheet?.kind === 'handoff' && <HandoffSheet row={sheet.row} open onClose={() => setSheet(null)} />}
    </>
  );
}

export function CheatSheet({ open, onClose, mac }: { open: boolean; onClose(): void; mac: boolean }) {
  return (
    <Sheet open={open} onClose={onClose} title={t.keys.title}>
      <dl className="grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-2 pb-2 text-sm">
        {SHORTCUTS.map((s) => (
          <div key={s.what} className="contents">
            <dt className="flex flex-wrap gap-1">
              {s.keys.map((k) => (
                <kbd key={k} className="min-w-6 rounded-md border border-border bg-bg px-1.5 py-0.5 text-center font-sans text-xs">
                  {keyLabel(mac, k)}
                </kbd>
              ))}
            </dt>
            <dd className="text-muted">{t.keys[s.what]}</dd>
          </div>
        ))}
      </dl>
    </Sheet>
  );
}
