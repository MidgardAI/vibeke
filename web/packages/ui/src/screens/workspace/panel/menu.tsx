// A small non-modal dropdown for the panel header (compare mode, branches, more): opens under its
// trigger, closes on outside pointer, Escape or a choice. Items are buttons (role=menuitem).

import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import { Check } from 'lucide-react';
import { cx } from '../../../components/ui';

export interface MenuItem {
  key: string;
  label: ReactNode;
  icon?: ReactNode;
  hint?: ReactNode;
  checked?: boolean;
  disabled?: boolean;
  onSelect?(): void;
}

export function Menu({
  trigger,
  label,
  items,
  align = 'left',
  className,
  triggerClassName,
}: {
  trigger: ReactNode;
  label: string;
  items: readonly MenuItem[];
  align?: 'left' | 'right';
  className?: string;
  triggerClassName?: string;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const down = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false);
        ref.current?.querySelector<HTMLElement>('[aria-haspopup]')?.focus();
      }
    };
    document.addEventListener('pointerdown', down, true);
    document.addEventListener('keydown', key, true);
    ref.current?.querySelector<HTMLElement>('[role=menuitem]:not([disabled])')?.focus();
    return () => {
      document.removeEventListener('pointerdown', down, true);
      document.removeEventListener('keydown', key, true);
    };
  }, [open]);
  const onMenuKey = (e: ReactKeyboardEvent) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp' && e.key !== 'j' && e.key !== 'k') return;
    e.preventDefault();
    e.stopPropagation();
    const list = [...(ref.current?.querySelectorAll<HTMLElement>('[role=menuitem]:not([disabled])') ?? [])];
    const at = list.indexOf(document.activeElement as HTMLElement);
    const d = e.key === 'ArrowDown' || e.key === 'j' ? 1 : -1;
    list[(at + d + list.length) % list.length]?.focus();
  };
  return (
    <div ref={ref} className={cx('relative', className)}>
      <button
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={typeof trigger === 'string' ? undefined : label}
        title={label}
        onClick={() => setOpen((v) => !v)}
        className={cx('vk-focus inline-flex h-7 items-center gap-1 rounded-md px-1.5 text-sm text-muted hover:bg-hover hover:text-fg aria-expanded:bg-hover aria-expanded:text-fg', triggerClassName)}
      >
        {trigger}
      </button>
      {open && (
        <div
          role="menu"
          aria-label={label}
          onKeyDown={onMenuKey}
          className={cx('absolute top-full z-40 mt-1 min-w-[200px] max-w-[300px] rounded-lg border border-border bg-surface p-1 shadow-[var(--shadow)]', align === 'right' ? 'right-0' : 'left-0')}
        >
          {items.map((it) => (
            <button
              key={it.key}
              type="button"
              role="menuitem"
              disabled={it.disabled}
              onClick={() => {
                setOpen(false);
                it.onSelect?.();
              }}
              className="vk-focus flex h-7 w-full items-center gap-2 rounded-md px-2 text-left text-sm text-fg/90 hover:bg-hover focus:bg-hover disabled:opacity-50 disabled:hover:bg-transparent pointer-coarse:h-9 [&>svg]:size-3.5"
            >
              <span className="flex size-3.5 shrink-0 items-center justify-center text-muted [&>svg]:size-3.5">{it.checked ? <Check /> : it.icon}</span>
              <span className="min-w-0 flex-1 truncate">{it.label}</span>
              {it.hint && <span className="shrink-0 text-xs text-faint">{it.hint}</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
