import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Activity, FileDiff, Inbox, Keyboard, Layers, Plus, Settings, Users } from 'lucide-react';
import { useApp, useHosts, useInboxItems, useNow, useTree } from './hooks';
import { emitUi, isMacLike } from './keyboard';
import { Button, Dot, IconButton, cx } from '../components/ui';
import { keyLabel } from '../lib/shortcuts';
import { t } from '../i18n';
import { BannerTracker } from '../lib/banner';
import { useStore } from '../lib/store';
import { navigate, type Route, type Tab } from '../router';

export function ConnectionBanner() {
  const app = useApp();
  const hosts = useHosts();
  const now = useNow(1000);
  const tracker = useRef(new BannerTracker());
  const b = tracker.current.update(hosts, now);
  if (b.level === 'none') return null;
  const fatalOnly = b.fatal.length > 0 && b.fatal.length === b.down.length;
  const tone = {
    amber: 'bg-warn/15 text-fg border-warn/40',
    red: 'bg-danger/15 text-fg border-danger/40',
    green: 'bg-ok/15 text-fg border-ok/40',
    none: '',
  }[b.level];
  const text =
    b.level === 'green'
      ? t.conn.back
      : b.level === 'amber'
        ? t.conn.reconnecting
        : fatalOnly
          ? `${b.fatal.join(', ')}: ${t.conn.revoked}`
          : t.conn.offline(b.down.join(', '));
  return (
    <div role="status" className={cx('flex min-h-9 items-center gap-2 border-b px-4 py-1 text-sm', tone)}>
      <span className="flex-1">{text}</span>
      {b.level === 'red' && !fatalOnly && (
        <Button size="sm" variant="outline" onClick={() => app.manager.connections().forEach((c) => c.reconnectNow())}>
          {t.retry}
        </Button>
      )}
      {fatalOnly && (
        <Button size="sm" variant="outline" onClick={() => navigate({ name: 'settings' })}>
          {t.settings.title}
        </Button>
      )}
    </div>
  );
}

export function Toasts() {
  const app = useApp();
  const toasts = useStore(app.toasts);
  if (!toasts.length) return null;
  return (
    <div className="pointer-events-none fixed inset-x-0 bottom-24 z-[70] flex flex-col items-center gap-2 px-4">
      {toasts.map((x) => (
        <div
          key={x.id}
          role="status"
          className={cx(
            'animate-in pointer-events-auto max-w-sm rounded-xl px-3.5 py-2 text-sm shadow-lg',
            x.tone === 'error' ? 'bg-danger text-danger-fg' : x.tone === 'ok' ? 'bg-ok text-ok-fg' : x.tone === 'warn' ? 'bg-warn text-black' : 'bg-fg text-bg',
          )}
        >
          {x.text}
        </div>
      ))}
    </div>
  );
}

function TabButton({ tab, label, icon, active, badge }: { tab: Tab; label: string; icon: ReactNode; active: boolean; badge?: number }) {
  return (
    <button
      type="button"
      onClick={() => navigate({ name: tab })}
      aria-current={active ? 'page' : undefined}
      className={cx('relative flex flex-1 flex-col items-center gap-0.5 border-t-2 pb-1 pt-1.5 text-2xs', active ? 'border-accent text-accent' : 'border-transparent text-muted')}
    >
      <span className="relative">
        {icon}
        {!!badge && (
          <span className="absolute -right-2.5 -top-1.5 min-w-4 rounded-full bg-need-strong px-1 text-center text-[10px] font-bold leading-4 text-black">{badge > 99 ? '99+' : badge}</span>
        )}
      </span>
      {label}
    </button>
  );
}

export function TabBar({ active }: { active?: Tab }) {
  const items = useInboxItems();
  const tree = useTree();
  return (
    <nav className="flex border-t border-border bg-surface pb-safe">
      <TabButton tab="inbox" label={t.tabs.inbox} icon={<Inbox className="size-5" />} active={active === 'inbox'} badge={items.length} />
      <TabButton tab="panes" label={t.tabs.panes} icon={<Layers className="size-5" />} active={active === 'panes'} />
      <TabButton tab="focus" label={t.tabs.focus} icon={<Activity className="size-5" />} active={active === 'focus'} badge={tree.needYou.length} />
      <TabButton tab="changes" label={t.tabs.changes} icon={<FileDiff className="size-5" />} active={active === 'changes'} />
    </nav>
  );
}

/** True while the window is wide enough for the sidebar layout (desktop, iPad landscape). */
export const WideContext = createContext(false);
export const useWide = (): boolean => useContext(WideContext);

export function useMediaQuery(query: string): boolean {
  const mq = typeof window !== 'undefined' && window.matchMedia ? window.matchMedia(query) : null;
  const [v, setV] = useState(mq?.matches ?? false);
  useEffect(() => {
    if (!mq) return;
    const f = () => setV(mq.matches);
    f();
    mq.addEventListener('change', f);
    return () => mq.removeEventListener('change', f);
  }, [query]);
  return v;
}

function SideItem({ label, icon, active, badge, onClick, hint }: { label: string; icon: ReactNode; active: boolean; badge?: number; onClick(): void; hint?: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? 'page' : undefined}
      title={hint ? `${label} (${hint})` : label}
      className={cx(
        'group flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-left text-sm focus-visible:outline-2 focus-visible:outline-accent',
        active ? 'bg-fg/10 font-medium text-fg' : 'text-fg/80 hover:bg-fg/5',
      )}
    >
      <span className={cx('shrink-0', active ? 'text-accent' : 'text-muted')}>{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {!!badge && <span className="min-w-5 rounded-full bg-need-strong px-1.5 text-center text-2xs font-semibold leading-[18px] text-black">{badge > 99 ? '99+' : badge}</span>}
      {!badge && hint && <span className="hidden text-2xs text-faint group-hover:inline">{hint}</span>}
    </button>
  );
}

/** Sidebar navigation for wide windows (replaces the bottom tab bar). */
export function Sidebar({ route }: { route: Route }) {
  const app = useApp();
  const items = useInboxItems();
  const tree = useTree();
  const mac = isMacLike(app.platform.mac);
  const k = (n: number) => keyLabel(mac, `mod+${n}`);
  return (
    <nav aria-label={t.nav.label} className="sidebar flex w-60 shrink-0 flex-col border-r border-border">
      <div className="titlebar flex h-11 shrink-0 items-center justify-end px-3" />
      <div className="space-y-0.5 px-2">
        <SideItem label={t.tabs.inbox} icon={<Inbox className="size-4" />} active={route.name === 'inbox' || route.name === 'interaction'} badge={items.length} hint={k(1)} onClick={() => navigate({ name: 'inbox' })} />
        <SideItem label={t.tabs.panes} icon={<Layers className="size-4" />} active={route.name === 'panes' || route.name === 'pane'} hint={k(2)} onClick={() => navigate({ name: 'panes' })} />
        <SideItem label={t.tabs.focus} icon={<Activity className="size-4" />} active={route.name === 'focus'} badge={tree.needYou.length} hint={k(3)} onClick={() => navigate({ name: 'focus' })} />
        <SideItem label={t.tabs.changes} icon={<FileDiff className="size-4" />} active={route.name === 'changes'} hint={k(4)} onClick={() => navigate({ name: 'changes' })} />
      </div>
      <div className="mt-5 flex items-center px-4 pb-1 text-2xs font-semibold uppercase tracking-wide text-faint">
        <span className="flex-1">{t.nav.hosts}</span>
        <button type="button" aria-label={t.settings.pairAnother} title={t.settings.pairAnother} className="rounded p-0.5 text-muted hover:text-fg" onClick={() => navigate({ name: 'pair', d: null })}>
          <Plus className="size-3.5" />
        </button>
      </div>
      <div className="min-h-0 flex-1 space-y-0.5 overflow-y-auto px-2">
        {tree.hosts.map((g) => (
          <button
            key={g.host.record.host_id}
            type="button"
            onClick={() => navigate({ name: 'crew' })}
            className="flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-left text-sm text-fg/80 hover:bg-fg/5"
          >
            <Dot tone={g.host.status === 'online' ? 'ok' : g.host.status === 'connecting' ? 'warn' : 'danger'} />
            <span className="min-w-0 flex-1 truncate">{g.host.info?.host_name ?? g.host.record.name}</span>
            {g.needsYou > 0 && <span className="text-2xs font-semibold text-need-strong">{g.needsYou}</span>}
          </button>
        ))}
      </div>
      <div className="space-y-0.5 border-t border-border p-2">
        <SideItem label={t.crew.title} icon={<Users className="size-4" />} active={route.name === 'crew'} onClick={() => navigate({ name: 'crew' })} />
        <SideItem label={t.settings.title} icon={<Settings className="size-4" />} active={route.name === 'settings'} hint={mac ? '⌘,' : undefined} onClick={() => navigate({ name: 'settings' })} />
        <SideItem label={t.keys.title!} icon={<Keyboard className="size-4" />} active={false} hint="?" onClick={() => emitUi('shortcuts')} />
      </div>
    </nav>
  );
}

export function TopBar({ title, route, column }: { title: string; route: Route; column?: string }) {
  const wide = useWide();
  const hosts = useHosts();
  const label = useMemo(() => {
    if (hosts.length === 1) {
      const h = hosts[0]!;
      return [h.info?.host_name ?? h.record.name, h.dashboard?.session].filter(Boolean).join(' · ');
    }
    return hosts.length ? `${hosts.filter((h) => h.status === 'online').length}/${hosts.length}` : '';
  }, [hosts]);
  return (
    <header className={cx('titlebar pb-2 pt-3', !wide && 'titlebar-inset')}>
      <div className={cx('flex items-center gap-2 px-4', column)}>
      <div className="min-w-0 flex-1">
        <h1 className="text-[22px] font-bold leading-tight tracking-tight">{title}</h1>
        {label && <div className="truncate text-xs text-muted">{label}</div>}
      </div>
      {!wide && (
        <>
          <IconButton label={t.crew.title} active={route.name === 'crew'} onClick={() => navigate({ name: 'crew' })}>
            <Users className="size-5" />
          </IconButton>
          <IconButton label={t.settings.title} active={route.name === 'settings'} onClick={() => navigate({ name: 'settings' })}>
            <Settings className="size-5" />
          </IconButton>
        </>
      )}
      </div>
    </header>
  );
}

/** Applies theme and terminal font prefs to the document root. */
export function useThemeEffect(theme: string, termFont: number): void {
  useEffect(() => {
    const root = document.documentElement;
    if (theme === 'system') root.removeAttribute('data-theme');
    else root.setAttribute('data-theme', theme);
    root.style.setProperty('--term-font', `${termFont}px`);
  }, [theme, termFont]);
}

/** Use a busy bar while any host is connecting with no dashboard yet. */
export function BusyBar() {
  const hosts = useHosts();
  const [show, setShow] = useState(false);
  const busy = hosts.some((h) => h.status === 'connecting' && !h.dashboard);
  useEffect(() => {
    if (!busy) return setShow(false);
    const id = setTimeout(() => setShow(true), 400);
    return () => clearTimeout(id);
  }, [busy]);
  if (!show) return null;
  return <div className="h-0.5 w-full animate-pulse bg-accent" />;
}
