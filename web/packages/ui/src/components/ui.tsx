// Primitives. Every clickable control with a label is a Button; floating layers are Dialogs (Sheet).
// State changes repaint only (borders are reserved transparent), so rows never jump.

import { useEffect, useId, useState, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode, type SVGProps } from 'react';
import { Bot, ChevronRight, Loader2, SquareTerminal, X } from 'lucide-react';
import type { Risk } from '@vibeke/core';
import { t } from '../i18n';
import { fmtCount, relTime } from '../lib/format';
import { Dialog } from './dialog';

export const cx = (...c: (string | false | null | undefined)[]): string => c.filter(Boolean).join(' ');

type Variant = 'primary' | 'secondary' | 'ghost' | 'danger' | 'ok' | 'outline';
type Size = 'sm' | 'md' | 'lg';

const VARIANTS: Record<Variant, string> = {
  primary: 'border-transparent bg-accent text-accent-fg hover:opacity-90',
  secondary: 'border-border bg-surface-2 text-fg hover:bg-surface-3',
  ghost: 'border-transparent bg-transparent text-fg hover:bg-hover',
  danger: 'border-transparent bg-danger text-danger-fg hover:opacity-90',
  ok: 'border-transparent bg-ok text-ok-fg hover:opacity-90',
  outline: 'border-border bg-transparent text-fg hover:bg-hover',
};
// Dense on pointer devices, comfortable touch targets on coarse pointers (phones, tablets).
const SIZES: Record<Size, string> = {
  sm: 'h-7 px-2.5 text-xs rounded-md gap-1.5 pointer-coarse:h-9 [&>svg]:size-3.5',
  md: 'h-8 px-3 text-sm rounded-md gap-1.5 pointer-coarse:h-10 [&>svg]:size-4',
  lg: 'h-10 px-4 text-base rounded-lg gap-2 pointer-coarse:h-12',
};

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  busy?: boolean;
  icon?: ReactNode;
  block?: boolean;
}

export function Button({ variant = 'secondary', size = 'md', busy, icon, block, className, children, disabled, ...rest }: ButtonProps) {
  return (
    <button
      type="button"
      {...rest}
      disabled={disabled || busy}
      className={cx(
        'inline-flex items-center justify-center border font-medium select-none whitespace-nowrap',
        'transition-[opacity,background-color] active:opacity-80 disabled:opacity-40 disabled:cursor-not-allowed',
        'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-fg/50',
        VARIANTS[variant],
        SIZES[size],
        block && 'w-full',
        className,
      )}
    >
      {busy ? <Loader2 className="size-4 animate-spin" aria-hidden /> : icon}
      {children}
    </button>
  );
}

export function IconButton({
  label,
  className,
  children,
  active,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string; active?: boolean }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      {...rest}
      className={cx(
        'inline-flex size-7 shrink-0 items-center justify-center rounded-md border border-transparent text-muted',
        'hover:bg-hover hover:text-fg active:bg-selected disabled:opacity-40 disabled:hover:bg-transparent focus-visible:outline-2 focus-visible:outline-fg/50',
        'pointer-fine:[&>svg]:size-4 pointer-coarse:size-10',
        active && 'bg-selected text-fg',
        className,
      )}
    >
      {children}
    </button>
  );
}

export function Spinner({ className }: { className?: string }) {
  return <Loader2 className={cx('size-4 animate-spin text-muted', className)} aria-label={t.loading} />;
}

const RISK_TONE: Record<Risk, string> = {
  low: 'text-ok border-ok/40',
  medium: 'text-warn border-warn/40',
  high: 'text-danger border-danger/50 bg-danger/10',
  unknown: 'text-danger border-danger/40',
};

export function RiskBadge({ risk }: { risk: Risk }) {
  return (
    <span className={cx('inline-flex h-[18px] items-center rounded-full border px-1.5 text-2xs font-medium', RISK_TONE[risk])}>
      {t.inbox.risk[risk] ?? risk}
    </span>
  );
}

export function Pill({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span className={cx('inline-flex h-[18px] items-center rounded-full bg-surface-2 px-1.5 text-2xs text-muted', className)}>
      {children}
    </span>
  );
}

export function Dot({ tone }: { tone: 'ok' | 'warn' | 'danger' | 'muted' | 'need' | 'accent' }) {
  const c = {
    ok: 'bg-ok',
    warn: 'bg-warn',
    danger: 'bg-danger',
    muted: 'bg-faint',
    need: 'bg-need-strong',
    accent: 'bg-accent',
  }[tone];
  return <span className={cx('inline-block size-2 shrink-0 rounded-full', c)} aria-hidden />;
}

export function Toggle({ checked, onChange, label, disabled }: { checked: boolean; onChange(v: boolean): void; label: string; disabled?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cx(
        'relative h-5 w-9 shrink-0 rounded-full border transition-colors disabled:opacity-40',
        checked ? 'border-transparent bg-add' : 'border-border-strong bg-surface-3',
      )}
    >
      <span
        className={cx(
          'absolute left-px top-px size-4 rounded-full bg-white shadow-sm transition-transform',
          checked ? 'translate-x-4' : 'translate-x-0',
        )}
      />
    </button>
  );
}

export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange(v: T): void;
  label: string;
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-md border border-border bg-surface p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          onClick={() => onChange(o.value)}
          className={cx(
            'h-6 rounded-[4px] border border-transparent px-2.5 text-xs pointer-coarse:h-8',
            value === o.value ? 'bg-surface-3 font-medium text-fg' : 'text-muted hover:text-fg',
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Notice({
  tone = 'info',
  children,
  action,
  className,
}: {
  tone?: 'info' | 'warn' | 'danger' | 'ok';
  children: ReactNode;
  action?: ReactNode;
  className?: string;
}) {
  const tones = {
    info: 'bg-surface-2 text-fg border-border',
    warn: 'bg-warn/10 text-fg border-warn/40',
    danger: 'bg-danger/10 text-fg border-danger/40',
    ok: 'bg-ok/10 text-fg border-ok/40',
  };
  return (
    <div role="status" className={cx('flex min-h-9 items-center gap-2 rounded-lg border px-3 py-1.5 text-sm', tones[tone], className)}>
      <div className="min-w-0 flex-1">{children}</div>
      {action}
    </div>
  );
}

export function Empty({ title, hint, icon, action }: { title: string; hint?: string; icon?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-1.5 px-6 py-16 text-center">
      {icon && <div className="mb-2 text-faint [&>svg]:size-8 [&>svg]:stroke-[1.5]">{icon}</div>}
      <div className="text-base font-medium">{title}</div>
      {hint && <div className="max-w-xs text-sm text-muted">{hint}</div>}
      {action && <div className="mt-3">{action}</div>}
    </div>
  );
}

export function SectionLabel({ children, right }: { children: ReactNode; right?: ReactNode }) {
  return (
    <div className="flex items-center justify-between px-4 pb-1.5 pt-5 text-xs font-medium text-muted sm:px-6">
      <span>{children}</span>
      {right}
    </div>
  );
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx('rounded-2xl border border-border bg-surface', className)}>{children}</div>;
}

/**
 * The standard floating layer: a bottom sheet on narrow screens, a centred panel on wide ones.
 * A modal `Dialog` (focus trapped, background inert, focus restored on close).
 */
export function Sheet({ open, onClose, title, children, role }: { open: boolean; onClose(): void; title?: ReactNode; children: ReactNode; role?: 'dialog' | 'alertdialog' }) {
  const titleId = useId();
  return (
    <Dialog
      open={open}
      onClose={onClose}
      role={role}
      labelledBy={title ? titleId : undefined}
      label={title ? undefined : t.close}
      dragDismiss
      closeOnNavigate
      className="fixed inset-0 z-50 flex flex-col justify-end sm:items-center sm:justify-center sm:p-6"
      panelClassName="animate-sheet vk-scroll relative max-h-[88vh] w-full overflow-y-auto rounded-t-2xl border-t border-border bg-surface pb-safe shadow-[var(--shadow)] outline-none sm:max-h-[80vh] sm:max-w-md sm:rounded-xl sm:border-0 sm:pb-0"
    >
      <div aria-hidden className="mx-auto mt-2 h-1 w-9 rounded-full bg-border-strong sm:hidden" />
      <div className="sticky top-0 z-10 flex items-center gap-2 bg-surface px-4 pb-1.5 pt-1.5 sm:pt-3">
        <h2 id={titleId} className="min-w-0 flex-1 truncate text-base font-semibold">
          {title}
        </h2>
        <IconButton label={t.close} onClick={onClose} className="-mr-1.5">
          <X className="size-4" />
        </IconButton>
      </div>
      <div className="px-4 pb-4">{children}</div>
    </Dialog>
  );
}

export function SheetRow({ icon, children, onClick, tone, disabled }: { icon?: ReactNode; children: ReactNode; onClick(): void; tone?: 'danger'; disabled?: boolean }) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cx(
        'flex h-9 w-full items-center gap-2.5 rounded-md border border-transparent px-2 text-left text-sm hover:bg-hover active:bg-selected disabled:opacity-40 pointer-coarse:h-11 pointer-coarse:text-base [&_svg]:size-4',
        tone === 'danger' && 'text-danger',
      )}
    >
      <span className="text-muted">{icon}</span>
      <span className="flex-1">{children}</span>
    </button>
  );
}

export function TextField(props: InputHTMLAttributes<HTMLInputElement> & { label?: string }) {
  const { label, className, ...rest } = props;
  return (
    <label className="block">
      {label && <span className="mb-1 block text-xs text-muted">{label}</span>}
      <input
        {...rest}
        className={cx(
          'h-8 w-full rounded-md border border-border bg-bg px-2.5 text-sm text-fg placeholder:text-faint pointer-coarse:h-10 pointer-coarse:text-base',
          'focus:border-border-strong focus:outline-none focus-visible:ring-2 focus-visible:ring-fg/15',
          className,
        )}
      />
    </label>
  );
}

// ---- dense workspace primitives ----------------------------------------------------------

/**
 * A list/tree row: 28px (24 compact) on pointer devices, 36px on touch. `active` is the current
 * route (filled); keyboard selection (`data-nav-item`) shows a hairline ring (styles.css).
 */
export function Row({
  active,
  compact,
  depth = 0,
  leading,
  trailing,
  sub,
  className,
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  active?: boolean;
  compact?: boolean;
  depth?: number;
  leading?: ReactNode;
  trailing?: ReactNode;
  /** Second line (muted), indented under the title. */
  sub?: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-current={active ? 'true' : undefined}
      {...rest}
      style={depth ? { paddingLeft: 8 + depth * 12, ...rest.style } : rest.style}
      className={cx(
        'vk-row vk-focus group flex w-full min-w-0 flex-col justify-center rounded-md px-2 text-left text-sm',
        compact ? 'min-h-[var(--row-h-compact)]' : 'min-h-[var(--row-h)]',
        sub ? 'py-1' : '',
        'pointer-coarse:min-h-9',
        active ? 'bg-selected text-fg' : 'text-fg/90 hover:bg-hover',
        className,
      )}
    >
      <span className="flex w-full min-w-0 items-center gap-2">
        {leading && <span className="flex size-4 shrink-0 items-center justify-center text-muted">{leading}</span>}
        <span className="min-w-0 flex-1 truncate">{children}</span>
        {trailing && <span className="flex shrink-0 items-center gap-1.5 text-xs text-faint">{trailing}</span>}
      </span>
      {sub && <span className={cx('flex w-full min-w-0 items-center gap-1 truncate text-xs text-muted', !!leading && 'pl-6')}>{sub}</span>}
    </button>
  );
}

/** Collapsible section heading with an optional icon, count and right-side controls. */
export function SectionHeader({
  title,
  icon,
  count,
  collapsed,
  onToggle,
  right,
  className,
}: {
  title: ReactNode;
  icon?: ReactNode;
  count?: number;
  collapsed?: boolean;
  onToggle?: () => void;
  right?: ReactNode;
  className?: string;
}) {
  const label = (
    <>
      {icon && <span className="flex size-4 shrink-0 items-center justify-center">{icon}</span>}
      <span className="truncate">{title}</span>
      {count !== undefined && count > 0 && <span className="tabular-nums text-faint">{count}</span>}
      {onToggle && <ChevronRight aria-hidden className={cx('size-3 shrink-0 text-faint opacity-0 transition-[transform,opacity] group-hover:opacity-100', !collapsed && 'rotate-90', collapsed && 'opacity-100')} />}
    </>
  );
  return (
    <div className={cx('group flex h-7 items-center gap-1 px-2 text-xs font-medium text-muted', className)}>
      {onToggle ? (
        <button type="button" aria-expanded={!collapsed} onClick={onToggle} className="vk-focus flex h-full min-w-0 flex-1 items-center gap-2 rounded-md text-left hover:text-fg">
          {label}
        </button>
      ) : (
        <div className="flex min-w-0 flex-1 items-center gap-2">{label}</div>
      )}
      {right}
    </div>
  );
}

/** `+1.9k −684`: additions green, deletions red, tabular. Nothing when both are zero. */
export function DiffCount({ adds, dels, className, always }: { adds: number; dels: number; className?: string; always?: boolean }) {
  if (!always && !adds && !dels) return null;
  return (
    <span className={cx('inline-flex shrink-0 gap-1 font-mono text-xs tabular-nums', className)} aria-label={`${adds} added, ${dels} removed`}>
      <span className="text-add">+{fmtCount(adds)}</span>
      <span className="text-del">−{fmtCount(dels)}</span>
    </span>
  );
}

/** Minute ticker shared by every RelTime (one interval per window). */
let tickNow = Date.now();
const tickSubs = new Set<(n: number) => void>();
let tickTimer: ReturnType<typeof setInterval> | null = null;
function useMinuteNow(): number {
  const [now, setNow] = useState(tickNow);
  useEffect(() => {
    tickSubs.add(setNow);
    if (!tickTimer)
      tickTimer = setInterval(() => {
        tickNow = Date.now();
        for (const f of tickSubs) f(tickNow);
      }, 30_000);
    setNow((tickNow = Date.now()));
    return () => {
      tickSubs.delete(setNow);
      if (!tickSubs.size && tickTimer) {
        clearInterval(tickTimer);
        tickTimer = null;
      }
    };
  }, []);
  return now;
}

/** `now`, `12m`, `3d`: compact age, full timestamp on hover. */
export function RelTime({ ms, now, className }: { ms: number; now?: number; className?: string }) {
  const tick = useMinuteNow();
  if (!ms) return null;
  const n = now ?? tick;
  return (
    <time dateTime={new Date(ms).toISOString()} title={new Date(ms).toLocaleString()} className={cx('shrink-0 text-xs tabular-nums text-faint', className)}>
      {relTime(ms, n)}
    </time>
  );
}

const glyph = (props: SVGProps<SVGSVGElement>) => ({
  viewBox: '0 0 16 16',
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 1.5,
  strokeLinecap: 'round' as const,
  strokeLinejoin: 'round' as const,
  'aria-hidden': true,
  ...props,
});

/** A small, neutral glyph per harness (not the vendors' logos); a terminal glyph for shells. */
export function HarnessIcon({ harness, className }: { harness: string | null | undefined; className?: string }) {
  const c = cx('size-3.5 shrink-0', className);
  switch (harness?.toLowerCase()) {
    case 'claude':
      return (
        <svg {...glyph({ className: cx(c, 'text-[#d97757]') })}>
          <path d="M8 2v12M2 8h12M3.8 3.8l8.4 8.4M12.2 3.8l-8.4 8.4" />
        </svg>
      );
    case 'codex':
      return (
        <svg {...glyph({ className: c })}>
          <path d="M8 1.8 13.4 4.9v6.2L8 14.2 2.6 11.1V4.9Z" />
          <path d="m5.6 6.4 1.8 1.6-1.8 1.6M8.6 10h2" />
        </svg>
      );
    case 'gemini':
      return (
        <svg {...glyph({ className: c })}>
          <path d="M8 1.5c.6 3.5 3 5.9 6.5 6.5-3.5.6-5.9 3-6.5 6.5-.6-3.5-3-5.9-6.5-6.5C5 7.4 7.4 5 8 1.5Z" />
        </svg>
      );
    case 'pi':
    case 'omp':
      return (
        <svg {...glyph({ className: c })}>
          <path d="M2.5 4.5h11M5.5 4.5v8.5M10.5 4.5V11c0 1.2.6 2 1.8 2" />
        </svg>
      );
    case undefined:
    case '':
      return <SquareTerminal aria-hidden className={c} strokeWidth={1.75} />;
    default:
      return <Bot aria-hidden className={c} strokeWidth={1.75} />;
  }
}

export type Status = 'need' | 'working' | 'review' | 'done' | 'idle' | 'error' | 'offline';

const STATUS_DOT: Record<Status, string> = {
  need: 'bg-need',
  working: 'bg-info vk-pulse',
  review: 'bg-add',
  done: 'bg-faint',
  idle: 'bg-transparent border border-faint',
  error: 'bg-del',
  offline: 'bg-transparent border border-del',
};

/** 6px status dot (working pulses). `ring` cuts it out of an icon it overlaps. */
export function StatusDot({ status, className, label, ring }: { status: Status; className?: string; label?: string; ring?: boolean }) {
  return (
    <span
      role={label ? 'img' : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      className={cx('inline-block size-1.5 shrink-0 rounded-full', STATUS_DOT[status], ring && 'box-content border-2 border-[var(--ring,var(--bg))]', className)}
    />
  );
}

/** Small rounded label: counts, filters, states. */
export function Chip({
  children,
  icon,
  tone = 'default',
  className,
  onClick,
  active,
  title,
}: {
  children: ReactNode;
  icon?: ReactNode;
  tone?: 'default' | 'need' | 'add' | 'del' | 'info';
  className?: string;
  onClick?: () => void;
  active?: boolean;
  title?: string;
}) {
  const tones = {
    default: 'border-border text-muted',
    need: 'border-need/40 bg-need-bg text-need',
    add: 'border-add/40 text-add',
    del: 'border-del/40 text-del',
    info: 'border-info/40 text-info',
  };
  const cls = cx(
    'inline-flex h-6 shrink-0 items-center gap-1 rounded-full border px-2 text-xs [&>svg]:size-3.5',
    tones[tone],
    active && 'border-border-strong bg-selected text-fg',
    onClick && 'vk-focus hover:bg-hover hover:text-fg',
    className,
  );
  return onClick ? (
    <button type="button" className={cls} onClick={onClick} aria-pressed={active} title={title}>
      {icon}
      {children}
    </button>
  ) : (
    <span className={cls} title={title}>
      {icon}
      {children}
    </span>
  );
}

/** Keycap: `⌘K`, `esc`. */
export function Kbd({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <kbd className={cx('inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-[4px] border border-border bg-surface-2 px-1 font-sans text-2xs leading-none text-muted', className)}>
      {children}
    </kbd>
  );
}

/** Small count badge (Inbox): amber pill, `99+` cap. */
export function Badge({ n, className }: { n: number; className?: string }) {
  if (!n) return null;
  return <span className={cx('inline-flex h-4 min-w-4 items-center justify-center rounded-full bg-need px-1 text-2xs font-semibold leading-none tabular-nums text-black', className)}>{n > 99 ? '99+' : n}</span>;
}

export interface TabItem<T extends string> {
  value: T;
  label: string;
  icon?: ReactNode;
  badge?: ReactNode;
  disabled?: boolean;
}

/** A compact tab strip (role=tablist): the selected tab is a filled pill. */
export function Tabs<T extends string>({ value, items, onChange, label, className }: { value: T; items: TabItem<T>[]; onChange(v: T): void; label: string; className?: string }) {
  return (
    <div role="tablist" aria-label={label} className={cx('flex min-w-0 items-center gap-0.5', className)}>
      {items.map((it) => (
        <button
          key={it.value}
          type="button"
          role="tab"
          aria-selected={value === it.value}
          disabled={it.disabled}
          onClick={() => onChange(it.value)}
          className={cx(
            'vk-focus inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2.5 text-sm disabled:opacity-40 [&>svg]:size-3.5',
            value === it.value ? 'bg-selected font-medium text-fg' : 'text-muted hover:bg-hover hover:text-fg',
          )}
        >
          {it.icon}
          {it.label}
          {it.badge}
        </button>
      ))}
    </div>
  );
}
