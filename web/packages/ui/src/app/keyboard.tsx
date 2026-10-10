// Keyboard layer (spec 16 §16.2): global shortcuts, the command palette, the `?` cheat sheet,
// and shell commands from native menus. Mounted once per window by <VibekeApp/>.

import { useEffect, useMemo, useRef, useState } from 'react';
import { CommandPalette, useEntityItems, type PaletteItem } from '../components/command-palette';
import { useCatchUpTracking } from '../lib/use-catch-up';
import { dialogOpen } from '../components/dialog';
import { NewSheet } from '../components/new-sheet';
import { Sheet } from '../components/ui';
import { UpdateSheet } from '../components/updates';
import { t } from '../i18n';
import { agentViewCommands, type AgentView } from '../lib/agent-view';
import { listNav, type NavAct } from '../lib/list-nav';
import { SHORTCUTS, keyLabel, shortcutFor, type ShortcutAction } from '../lib/shortcuts';
import type { PaneRow } from '../lib/tree';
import type { UiCommand } from '../platform';
import { formatRoute, goBack, navigate, useRoute, workspaceRoute, type Route } from '../router';
import { CloudAuthHost } from '../components/cloud-auth';
import { CloudSheet } from '../screens/cloud-send';
import { HandoffSheet } from '../screens/handoff';
import { isOwnFullHost } from '../lib/handoff-send';
import { ShareSheet } from '../screens/share';
import { useApp, usePrefs, useTree } from './hooks';
import { useTogglePanel } from './layout';
import { currentLayoutMode, drawerOpen, lastWorkspace, selectedPane, useWorkspaceRows, useWorkspaces } from './selection';

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

/** What a workspace screen is asked to show its agents as (`default` clears the override). */
export type AgentViewRequest = AgentView | 'toggle' | 'default';
const viewBus = new Set<(r: AgentViewRequest) => void>();
/** The mounted workspace screen listens; returns an unsubscribe. */
export function onAgentViewRequest(cb: (r: AgentViewRequest) => void): () => void {
  viewBus.add(cb);
  return () => viewBus.delete(cb);
}
/** Ask the workspace on screen to switch its agent view; false when none is mounted. */
export function requestAgentView(r: AgentViewRequest): boolean {
  for (const f of [...viewBus]) f(r);
  return viewBus.size > 0;
}

/** Set while the shell is mounted: opens the cloud sheet at the app level, so it outlives the
 * workspace screen whose pane the move closes. */
let cloudOpener: ((mode: 'send' | 'bring_back', row: PaneRow) => void) | null = null;

/** Open "Send to cloud" or "Bring back" for `row` (the workspace header's cloud button). */
export function openCloudSheet(mode: 'send' | 'bring_back', row: PaneRow): void {
  cloudOpener?.(mode, row);
}

type SheetState = { kind: 'new' } | { kind: 'share'; row: PaneRow } | { kind: 'handoff'; row: PaneRow } | { kind: 'cloud_send'; row: PaneRow } | { kind: 'cloud_back'; host: string; pane?: string } | null;

const TYPING = /^(INPUT|TEXTAREA|SELECT)$/;
const SUBPAGES = new Set<Route['name']>(['pane', 'interaction', 'run', 'settings', 'crew', 'sandboxes', 'pair', 'not_found']);

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
  const [updates, setUpdates] = useState(false);
  const [sheet, setSheet] = useState<SheetState>(null);
  useEffect(() => {
    cloudOpener = (mode, row) => setSheet(mode === 'send' ? { kind: 'cloud_send', row } : { kind: 'cloud_back', host: row.host, pane: row.pane.id });
    return () => {
      cloudOpener = null;
    };
  }, []);
  const routeKey = formatRoute(route);
  const rows = useWorkspaceRows();
  const sidebar = useWorkspaces();
  const togglePanel = useTogglePanel();

  useEffect(() => listNav.clear(), [routeKey]);
  useEffect(() => listNav.attach(), []);

  const wsRow = route.name === 'workspace' ? rows.find((r) => r.host === route.host && r.workspace.id === route.workspace) : undefined;
  const paneId = route.name === 'pane' ? route.pane : route.name === 'workspace' ? selectedPane(route, wsRow) : null;
  const paneHost = route.name === 'pane' || route.name === 'workspace' ? route.host : null;
  const paneRow = paneId ? tree.all.find((r) => r.host === paneHost && r.pane.id === paneId) : undefined;

  // Shell actions shared by keys, menus and buttons (full window only).
  const actions = {
    firstWorkspace: () => {
      const first = sidebar.pinned[0] ?? sidebar.groups[0]?.rows[0] ?? rows[0];
      if (first) navigate(workspaceRoute(first.host, first.workspace.id));
      return !!first;
    },
    panel: () => {
      if (route.name !== 'workspace') return false;
      togglePanel(route, 'changes');
      return true;
    },
    sidebar: () => {
      if (currentLayoutMode() === 'narrow') drawerOpen.set(!drawerOpen.get());
      else app.prefs.patch({ sidebarHidden: !app.prefs.get().sidebarHidden });
      return true;
    },
    changes: () => {
      if (route.name === 'workspace') {
        navigate({ ...route, panel: 'changes', file: null, commit: null }, { replace: true });
        return true;
      }
      const last = lastWorkspace();
      const target = (last && rows.find((r) => r.host === last.host && r.workspace.id === last.ws)) || rows[0];
      if (target) navigate(workspaceRoute(target.host, target.workspace.id, { panel: 'changes' }));
      return !!target;
    },
  };
  const actionsRef = useRef(actions);
  actionsRef.current = actions;

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
        case 'go':
          if (surface !== 'full') return false;
          if (a.to === 'workspace') return actionsRef.current.firstWorkspace();
          navigate({ name: a.to });
          return true;
        case 'panel':
          return surface === 'full' && actionsRef.current.panel();
        case 'sidebar':
          return surface === 'full' && actionsRef.current.sidebar();
        case 'changes':
          return surface === 'full' && actionsRef.current.changes();
        case 'agentView':
          return surface !== 'quick' && requestAgentView('toggle');
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
          case 'updates':
            return setUpdates(true);
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
          case 'settings':
            return navigate({ name: cmd });
          case 'changes':
            return void actionsRef.current.changes();
          case 'workspace':
            return void actionsRef.current.firstWorkspace();
          case 'panel':
            return void actionsRef.current.panel();
          case 'sidebar':
            return void actionsRef.current.sidebar();
          case 'pair':
            return navigate({ name: 'pair', d: null });
          case 'back':
            return goBack({ name: 'home' });
          case 'pop-out':
            if (paneHost && paneId) app.platform.windows?.popOutPane?.(paneHost, paneId);
            return;
          case 'agent-view':
            return void requestAgentView('toggle');
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
    pane: (host, pane) => {
      const ws = tree.all.find((r) => r.host === host && r.pane.id === pane)?.pane.workspace;
      navigate(ws ? workspaceRoute(host, ws, { pane }) : { name: 'pane', host, pane, view: 'term' });
    },
    workspace: (host, ws) => navigate(workspaceRoute(host, ws)),
    host: () => navigate({ name: 'crew' }),
    interaction: (host, id) => navigate({ name: 'interaction', host, id, preselect: null }),
  });

  const commands = useMemo((): PaletteItem[] => {
    const mod = (k: string) => keyLabel(mac, `mod+${k}`);
    const c = (id: string, title: string, run: () => void, shortcut?: string, keywords?: string): PaletteItem => ({ id: `c:${id}`, group: 'command', title, run, shortcut, keywords });
    const out: PaletteItem[] = [
      c('inbox', `${t.palette.go} ${t.tabs.inbox}`, () => navigate({ name: 'inbox' }), mod('1')),
      c('workspace', t.palette.firstWorkspace, () => actionsRef.current.firstWorkspace(), mod('2'), 'workspaces panes'),
      c('panel', t.palette.togglePanel, () => actionsRef.current.panel(), mod('3'), 'changes diff'),
      c('changes', t.palette.showChanges, () => actionsRef.current.changes(), keyLabel(mac, 'mod+shift+e'), 'diff git'),
      c('sidebar', t.palette.toggleSidebar, () => actionsRef.current.sidebar(), mod('\\'), 'navigation'),
      c('new', t.palette.newAgent, () => setSheet({ kind: 'new' }), undefined, 'start agent tab'),
      c('pair', t.palette.pair, () => navigate({ name: 'pair', d: null }), undefined, 'connect link qr'),
      c('settings', t.palette.settings, () => navigate({ name: 'settings' }), mod('4'), 'preferences'),
      c('shortcuts', t.palette.shortcuts, () => setHelp(true), '?', 'keys help'),
      c('theme', t.palette.theme(prefs.theme === 'dark' ? 'light' : 'dark'), () => app.prefs.patch({ theme: prefs.theme === 'dark' ? 'light' : 'dark' }), undefined, 'dark light appearance'),
    ];
    if (app.platform.updates) out.push(c('updates', 'Check for updates…', () => { setUpdates(true); void app.platform.updates!.check(); }, undefined, 'update upgrade release'));
    // Lock pauses polling until "Resume": only the main window has that overlay.
    if (surface === 'full') out.push(c('lock', t.palette.lock, () => app.locked.set(true)));
    // Cloud sandboxes (spec 17): full-scope hosts only.
    const cloudHost = app.manager.getSnapshot().find(isOwnFullHost)?.record.host_id;
    if (cloudHost) {
      const paneFull = paneRow && (app.conn(paneRow.host)?.getSnapshot().info?.scope ?? 'view') === 'full';
      if (paneRow && paneFull) out.unshift(c('cloud-send', t.palette.cloudSend, () => setSheet({ kind: 'cloud_send', row: paneRow }), undefined, 'sandbox remote move'));
      out.unshift(c('cloud-back', t.palette.cloudBringBack, () => setSheet({ kind: 'cloud_back', host: paneFull ? paneRow!.host : cloudHost, ...(paneFull ? { pane: paneRow!.pane.id } : {}) }), undefined, 'sandbox remote return'));
      out.push(c('sandboxes', t.palette.sandboxes, () => navigate({ name: 'sandboxes' }), undefined, 'cloud boxes machines'));
    }
    if (paneRow) {
      const full = paneRow && (app.conn(paneRow.host)?.getSnapshot().info?.scope ?? 'view') === 'full';
      if (full) {
        out.unshift(c('handoff', t.palette.handoff, () => setSheet({ kind: 'handoff', row: paneRow }), undefined, 'move'));
        out.unshift(c('share', t.palette.share, () => setSheet({ kind: 'share', row: paneRow }), undefined, 'invite'));
      }
      // Agent view of the pane's workspace (only where it has agents).
      const ws = paneRow.pane.workspace;
      const hasAgent = ws && tree.all.some((r) => r.host === paneRow.host && r.pane.workspace === ws && r.run);
      if (ws && hasAgent) {
        const titles = { 'view-conversation': t.palette.showAsConversation, 'view-terminal': t.palette.showAsTerminal, 'view-default': t.palette.viewDefault };
        for (const v of agentViewCommands(prefs, paneRow.host, ws))
          out.push(c(v.id, titles[v.id], () => requestAgentView(v.request), v.flip ? keyLabel(mac, 'mod+shift+t') : undefined, 'agent view conversation terminal tui chat'));
      }
      const pop = app.platform.windows?.popOutPane;
      if (pop && surface === 'full') out.unshift(c('popout', t.palette.popOut, () => pop(paneRow.host, paneRow.pane.id), mac ? '⇧⌘O' : 'Ctrl+Shift+O', 'window'));
    }
    for (const x of app.platform.extensions?.commands?.() ?? []) out.push({ id: `x:${x.id}`, group: 'command', title: x.title, sub: x.hint, keywords: x.keywords, run: x.run });
    return out;
  }, [mac, prefs.theme, prefs.agentView, prefs.agentViews, paneRow, tree, app, surface]);

  if (surface === 'quick') {
    return <CheatSheet open={help} onClose={() => setHelp(false)} mac={mac} />;
  }
  return (
    <>
      {surface === 'full' && <CatchUpTracker />}
      <UpdateSheet open={updates} onClose={() => setUpdates(false)} />
      <CommandPalette open={palette} onClose={() => setPalette(false)} items={surface === 'full' ? [...commands, ...entities] : commands} />
      <CheatSheet open={help} onClose={() => setHelp(false)} mac={mac} />
      <NewSheet open={sheet?.kind === 'new'} onClose={() => setSheet(null)} />
      {sheet?.kind === 'share' && <ShareSheet row={sheet.row} open onClose={() => setSheet(null)} />}
      {sheet?.kind === 'handoff' && <HandoffSheet row={sheet.row} open onClose={() => setSheet(null)} />}
      {sheet?.kind === 'cloud_send' && <CloudSheet mode="send" host={sheet.row.host} pane={sheet.row.pane.id} open onClose={() => setSheet(null)} />}
      {sheet?.kind === 'cloud_back' && <CloudSheet mode="bring_back" host={sheet.host} pane={sheet.pane} open onClose={() => setSheet(null)} />}
      {surface === 'full' && <CloudAuthHost />}
    </>
  );
}

/** Records when the main window goes to the background (for the inbox's catch-up cards). */
function CatchUpTracker(): null {
  useCatchUpTracking();
  return null;
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
