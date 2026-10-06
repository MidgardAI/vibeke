import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Activity, FileDiff, Inbox, Layers, Settings, Users } from 'lucide-react';
import { useApp, useHosts, useInboxItems, useNow, useTree } from './hooks';
import { Button, IconButton, cx } from '../components/ui';
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
    <div role="status" className={cx('flex min-h-9 items-center gap-2 border-b px-4 py-1 text-[13px]', tone)}>
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
            'animate-in pointer-events-auto max-w-sm rounded-xl px-3.5 py-2 text-[13px] shadow-lg',
            x.tone === 'error' ? 'bg-danger text-white' : x.tone === 'ok' ? 'bg-ok text-white' : x.tone === 'warn' ? 'bg-warn text-black' : 'bg-fg text-bg',
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
      className={cx('relative flex flex-1 flex-col items-center gap-0.5 border-t-2 pb-1 pt-1.5 text-[11px]', active ? 'border-accent text-accent' : 'border-transparent text-muted')}
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

export function TopBar({ title, route }: { title: string; route: Route }) {
  const hosts = useHosts();
  const label = useMemo(() => {
    if (hosts.length === 1) {
      const h = hosts[0]!;
      return [h.info?.host_name ?? h.record.name, h.dashboard?.session].filter(Boolean).join(' · ');
    }
    return hosts.length ? `${hosts.filter((h) => h.status === 'online').length}/${hosts.length}` : '';
  }, [hosts]);
  return (
    <header className="flex items-center gap-2 px-4 pb-1 pt-2">
      <div className="min-w-0 flex-1">
        <div className="text-lg font-semibold leading-tight">{title}</div>
        {label && <div className="truncate text-[12px] text-muted">{label}</div>}
      </div>
      <IconButton label={t.crew.title} active={route.name === 'crew'} onClick={() => navigate({ name: 'crew' })}>
        <Users className="size-5" />
      </IconButton>
      <IconButton label={t.settings.title} active={route.name === 'settings'} onClick={() => navigate({ name: 'settings' })}>
        <Settings className="size-5" />
      </IconButton>
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
