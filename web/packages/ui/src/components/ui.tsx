// Primitives. Every clickable control with a label is a Button; the only floating layer is Sheet.
// State changes repaint only (borders are reserved transparent), so rows never jump.

import { useEffect, useRef, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode } from "react";
import { Loader2, X } from 'lucide-react';
import type { Risk } from '@vibeke/core';
import { t } from '../i18n';

export const cx = (...c: (string | false | null | undefined)[]): string => c.filter(Boolean).join(' ');

type Variant = 'primary' | 'secondary' | 'ghost' | 'danger' | 'ok' | 'outline';
type Size = 'sm' | 'md' | 'lg';

const VARIANTS: Record<Variant, string> = {
  primary: 'border-transparent bg-accent text-accent-fg',
  secondary: 'border-transparent bg-surface-2 text-fg',
  ghost: 'border-transparent bg-transparent text-fg',
  danger: 'border-transparent bg-danger text-white',
  ok: 'border-transparent bg-ok text-white',
  outline: 'border-border bg-transparent text-fg',
};
const SIZES: Record<Size, string> = {
  sm: 'h-8 px-2.5 text-[13px] rounded-lg gap-1.5',
  md: 'h-10 px-3.5 text-sm rounded-xl gap-2',
  lg: 'h-12 px-4 text-base rounded-xl gap-2',
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
        'transition-[opacity,background-color] active:opacity-70 disabled:opacity-40 disabled:cursor-not-allowed',
        'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent',
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
        'inline-flex size-10 shrink-0 items-center justify-center rounded-xl border border-transparent text-fg',
        'active:bg-surface-2 disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-accent',
        active && 'bg-surface-2 text-accent',
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
    <span className={cx('inline-flex h-5 items-center rounded-full border px-1.5 text-[11px] font-medium', RISK_TONE[risk])}>
      {t.inbox.risk[risk] ?? risk}
    </span>
  );
}

export function Pill({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span className={cx('inline-flex h-5 items-center rounded-full bg-surface-2 px-1.5 text-[11px] text-muted', className)}>
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
        'relative h-7 w-12 shrink-0 rounded-full border border-transparent transition-colors disabled:opacity-40',
        checked ? 'bg-accent' : 'bg-surface-2 border-border',
      )}
    >
      <span
        className={cx(
          'absolute left-0.5 top-0.5 size-5.5 rounded-full bg-white shadow transition-transform',
          checked ? 'translate-x-5' : 'translate-x-0',
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
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-xl bg-surface-2 p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          onClick={() => onChange(o.value)}
          className={cx(
            'h-8 rounded-[10px] border border-transparent px-3 text-[13px]',
            value === o.value ? 'bg-surface text-fg shadow-sm border-border' : 'text-muted',
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
    <div role="status" className={cx('flex min-h-10 items-center gap-2 rounded-xl border px-3 py-2 text-[13px]', tones[tone], className)}>
      <div className="min-w-0 flex-1">{children}</div>
      {action}
    </div>
  );
}

export function Empty({ title, hint, icon, action }: { title: string; hint?: string; icon?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-6 py-16 text-center">
      {icon && <div className="mb-1 text-faint">{icon}</div>}
      <div className="text-base font-medium">{title}</div>
      {hint && <div className="max-w-xs text-sm text-muted">{hint}</div>}
      {action && <div className="mt-3">{action}</div>}
    </div>
  );
}

export function SectionLabel({ children, right }: { children: ReactNode; right?: ReactNode }) {
  return (
    <div className="flex items-center justify-between px-4 pb-1.5 pt-4 text-[11px] font-semibold uppercase tracking-wide text-faint">
      <span>{children}</span>
      {right}
    </div>
  );
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx('rounded-2xl border border-border bg-surface', className)}>{children}</div>;
}

/** The only floating layer: a bottom sheet with a scrim. */
export function Sheet({ open, onClose, title, children }: { open: boolean; onClose(): void; title?: ReactNode; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    ref.current?.focus();
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div className="fixed inset-0 z-50 flex flex-col justify-end" role="dialog" aria-modal="true">
      <button type="button" aria-label={t.close} className="absolute inset-0 bg-black/40" onClick={onClose} />
      <div
        ref={ref}
        tabIndex={-1}
        className="animate-sheet relative max-h-[88vh] overflow-y-auto rounded-t-3xl border-t border-border bg-surface pb-safe outline-none"
      >
        <div className="sticky top-0 z-10 flex items-center gap-2 bg-surface px-4 pb-2 pt-3">
          <div className="min-w-0 flex-1 truncate text-base font-semibold">{title}</div>
          <IconButton label={t.close} onClick={onClose} className="-mr-2">
            <X className="size-5" />
          </IconButton>
        </div>
        <div className="px-4 pb-4">{children}</div>
      </div>
    </div>
  );
}

export function SheetRow({ icon, children, onClick, tone, disabled }: { icon?: ReactNode; children: ReactNode; onClick(): void; tone?: 'danger'; disabled?: boolean }) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cx(
        'flex h-12 w-full items-center gap-3 rounded-xl border border-transparent px-2 text-left text-[15px] active:bg-surface-2 disabled:opacity-40',
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
      {label && <span className="mb-1 block text-[13px] text-muted">{label}</span>}
      <input
        {...rest}
        className={cx(
          'h-11 w-full rounded-xl border border-border bg-bg px-3 text-[15px] text-fg placeholder:text-faint',
          'focus:outline-2 focus:outline-accent',
          className,
        )}
      />
    </label>
  );
}
