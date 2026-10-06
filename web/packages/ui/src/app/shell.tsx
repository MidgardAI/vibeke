import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { Menu } from 'lucide-react';
import { useApp, useHosts, useNow } from './hooks';
import { emitUi, isMacLike } from './keyboard';
import { Button, IconButton, cx } from '../components/ui';
import { keyLabel } from '../lib/shortcuts';
import { t } from '../i18n';
import { BannerTracker } from '../lib/banner';
import { useStore } from '../lib/store';
import { navigate } from '../router';

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
    <div role="status" className={cx('flex min-h-8 shrink-0 items-center gap-2 border-b px-3 py-1 text-xs', tone)}>
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
    <div className="pointer-events-none fixed inset-x-0 bottom-6 z-[70] flex flex-col items-center gap-2 px-4 pb-safe">
      {toasts.map((x) => (
        <div
          key={x.id}
          role="status"
          className={cx(
            'animate-in pointer-events-auto max-w-sm rounded-lg px-3 py-1.5 text-sm shadow-[var(--shadow)]',
            x.tone === 'error' ? 'bg-danger text-danger-fg' : x.tone === 'ok' ? 'bg-surface-3 text-fg' : x.tone === 'warn' ? 'bg-warn text-black' : 'bg-surface-3 text-fg',
          )}
        >
          {x.text}
        </div>
      ))}
    </div>
  );
}

/** True while the sidebar is shown inline (wide windows with the sidebar not hidden). */
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

/** Opens the sidebar drawer (narrow windows) or shows the hidden sidebar (wide windows). */
export function MenuButton() {
  const app = useApp();
  const mac = isMacLike(app.platform.mac);
  return (
    <IconButton label={`${t.sidebar.open} (${keyLabel(mac, 'mod+\\')})`} onClick={() => emitUi('sidebar')}>
      <Menu />
    </IconButton>
  );
}

/**
 * The centre column's title bar: 44px, title + muted subtitle, actions on the right. Without an
 * inline sidebar it leads with the menu button (and clears the macOS traffic lights).
 */
export function TopBar({ title, sub, right, leading }: { title: ReactNode; sub?: ReactNode; right?: ReactNode; leading?: ReactNode }) {
  const wide = useWide();
  return (
    <header className={cx('titlebar flex h-11 shrink-0 items-center gap-2 border-b border-border px-3', !wide && 'titlebar-inset pl-1.5')}>
      {!wide && <MenuButton />}
      {leading}
      <div className="flex min-w-0 flex-1 items-baseline gap-2">
        <h1 className="truncate text-base font-semibold">{title}</h1>
        {sub && <span className="truncate text-sm text-muted">{sub}</span>}
      </div>
      {right}
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
  return <div className="h-px w-full shrink-0 animate-pulse bg-info" />;
}
