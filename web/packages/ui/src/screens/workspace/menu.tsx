// A small dropdown menu anchored to its button (⋯, +): role=menu, arrow keys move, Escape and
// outside clicks close, focus returns to the button.

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { IconButton, cx } from '../../components/ui';

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  onSelect(): void;
  disabled?: boolean;
  tone?: 'danger';
  /** Shown on the right (shortcut, hint). */
  hint?: ReactNode;
}

export function MenuButton({
  label,
  icon,
  items,
  align = 'right',
  placement = 'down',
  className,
  header,
}: {
  label: string;
  icon: ReactNode;
  items: (MenuItem | 'sep' | null | false)[];
  align?: 'left' | 'right';
  /** `up`: opens above the button (composer). */
  placement?: 'down' | 'up';
  className?: string;
  header?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  const list = items.filter((x): x is MenuItem | 'sep' => !!x);
  return (
    <div ref={wrap} className={cx('relative', className)}>
      <IconButton label={label} aria-haspopup="menu" aria-expanded={open} active={open} onClick={() => setOpen(!open)}>
        {icon}
      </IconButton>
      {open && (
        <MenuList
          label={label}
          items={list}
          align={align}
          placement={placement}
          header={header}
          onClose={(refocus) => {
            setOpen(false);
            if (refocus) wrap.current?.querySelector<HTMLElement>('button')?.focus();
          }}
        />
      )}
    </div>
  );
}

function MenuList({
  label,
  items,
  align,
  placement,
  header,
  onClose,
}: {
  label: string;
  items: (MenuItem | 'sep')[];
  align: 'left' | 'right';
  placement: 'down' | 'up';
  header?: ReactNode;
  onClose(refocus: boolean): void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = ref.current;
    el?.querySelector<HTMLElement>('[role="menuitem"]:not(:disabled)')?.focus();
    const down = (e: PointerEvent) => {
      const target = e.target as Element;
      if (el && !el.contains(target) && !el.parentElement?.contains(target)) onClose(false);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        onClose(true);
        return;
      }
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
      const all = [...(el?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)') ?? [])];
      if (!all.length) return;
      e.preventDefault();
      e.stopPropagation();
      const i = all.indexOf(document.activeElement as HTMLElement);
      const next = e.key === 'ArrowDown' ? (i + 1) % all.length : (i - 1 + all.length) % all.length;
      all[next]!.focus();
    };
    document.addEventListener('pointerdown', down, true);
    document.addEventListener('keydown', key, true);
    return () => {
      document.removeEventListener('pointerdown', down, true);
      document.removeEventListener('keydown', key, true);
    };
  }, [onClose]);
  return (
    <div
      ref={ref}
      role="menu"
      aria-label={label}
      className={cx('animate-pop absolute z-40 min-w-52 max-w-72 rounded-lg bg-surface-2 p-1 text-fg shadow-[var(--shadow)]', placement === 'up' ? 'bottom-9' : 'top-8', align === 'right' ? 'right-0' : 'left-0')}
    >
      {header}
      {items.map((it, i) =>
        it === 'sep' ? (
          <div key={`sep${i}`} className="my-1 border-t border-border" />
        ) : (
          <button
            key={it.label}
            type="button"
            role="menuitem"
            disabled={it.disabled}
            onClick={() => {
              onClose(false);
              it.onSelect();
            }}
            className={cx(
              'vk-focus flex h-7 w-full items-center gap-2 rounded-[5px] px-2 text-left text-sm hover:bg-hover disabled:opacity-40 disabled:hover:bg-transparent pointer-coarse:h-10 [&_svg]:size-3.5',
              it.tone === 'danger' ? 'text-del' : 'text-fg/90',
            )}
          >
            {it.icon && <span className={cx('flex size-4 items-center justify-center', it.tone === 'danger' ? 'text-del' : 'text-muted')}>{it.icon}</span>}
            <span className="min-w-0 flex-1 truncate">{it.label}</span>
            {it.hint && <span className="shrink-0 text-xs text-faint">{it.hint}</span>}
          </button>
        ),
      )}
    </div>
  );
}
