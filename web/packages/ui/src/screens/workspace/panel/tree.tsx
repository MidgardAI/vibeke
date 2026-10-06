// Tree list shared by the Changes and Files tabs: indent guides, chevrons for folders, file
// icons, right-aligned counts and a status square. One row is tabbable (roving focus); j/k or
// ↑/↓ move, → opens a folder (or steps into it), ← closes it (or steps out), Enter activates.

import { useEffect, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from 'react';
import { ChevronRight } from 'lucide-react';
import { cx } from '../../../components/ui';
import { t } from '../../../i18n';

export interface TreeItem {
  key: string;
  depth: number;
  dir: boolean;
  /** Folder expanded. */
  open?: boolean;
  /** Parent folder key (← target). */
  parent: string | null;
  name: ReactNode;
  /** File icon (files) — folders show the chevron. */
  icon?: ReactNode;
  trailing?: ReactNode;
  /** Status square column (files); folders keep the column empty so counts align. */
  status?: ReactNode;
  dim?: boolean;
  active?: boolean;
  title?: string;
  /** Not openable (secret files…): no click action, still focusable. */
  inert?: boolean;
}

const PAD = 8;
const INDENT = 14;

export function TreeList({
  items,
  label,
  onToggle,
  onActivate,
  statusColumn = true,
  className,
}: {
  items: readonly TreeItem[];
  label: string;
  onToggle(item: TreeItem, open?: boolean): void;
  onActivate(item: TreeItem, e: MouseEvent | KeyboardEvent): void;
  statusColumn?: boolean;
  className?: string;
}) {
  const [focus, setFocus] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const current = items.find((i) => i.key === focus) ?? items.find((i) => i.active) ?? items[0];

  const focusKey = (key: string) => {
    setFocus(key);
    const el = ref.current?.querySelector<HTMLElement>(`[data-tree-key="${CSS.escape(key)}"]`);
    el?.focus({ preventScroll: true });
    el?.scrollIntoView?.({ block: 'nearest' });
  };

  useEffect(() => {
    if (focus && !items.some((i) => i.key === focus)) setFocus(null);
  }, [items, focus]);

  // Back from a diff or viewer: the tree remounts; put focus on its current row if it was lost.
  useEffect(() => {
    if (document.activeElement && document.activeElement !== document.body) return;
    ref.current?.querySelector<HTMLElement>('[tabindex="0"]')?.focus({ preventScroll: true });
  }, []);

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.metaKey || e.ctrlKey || !current) return;
    const at = items.indexOf(current);
    const go = (i: number) => {
      const next = items[Math.max(0, Math.min(items.length - 1, i))];
      if (next) focusKey(next.key);
    };
    switch (e.key) {
      case 'j':
      case 'ArrowDown':
        go(at + 1);
        break;
      case 'k':
      case 'ArrowUp':
        go(at - 1);
        break;
      case 'Home':
        go(0);
        break;
      case 'End':
        go(items.length - 1);
        break;
      case 'l':
      case 'ArrowRight':
        if (!current.dir) return;
        if (current.open) go(at + 1);
        else onToggle(current, true);
        break;
      case 'h':
      case 'ArrowLeft':
        if (current.dir && current.open) onToggle(current, false);
        else if (current.parent) focusKey(current.parent);
        else return;
        break;
      case 'Enter':
        if (current.dir) onToggle(current);
        else if (!current.inert) onActivate(current, e);
        break;
      default:
        return;
    }
    e.preventDefault();
    e.stopPropagation();
  };

  return (
    <div ref={ref} role="tree" aria-label={label} onKeyDown={onKeyDown} className={cx('py-1', className)}>
      {items.map((it) => {
        const pad = PAD + it.depth * INDENT;
        return (
          <button
            key={it.key}
            type="button"
            role="treeitem"
            aria-level={it.depth + 1}
            aria-expanded={it.dir ? !!it.open : undefined}
            aria-selected={it.active ? true : undefined}
            aria-current={it.active ? 'true' : undefined}
            data-tree-key={it.key}
            tabIndex={it === current ? 0 : -1}
            title={it.title}
            onFocus={() => setFocus(it.key)}
            onClick={(e) => {
              setFocus(it.key);
              if (it.dir) onToggle(it);
              else if (!it.inert) onActivate(it, e);
            }}
            style={{ paddingLeft: pad }}
            className={cx(
              'vk-focus group relative flex h-[var(--row-h)] w-full min-w-0 items-center gap-1.5 pr-2 text-left text-sm pointer-coarse:h-9',
              it.active ? 'bg-selected text-fg' : 'text-fg/90 hover:bg-hover',
              it.dim && 'opacity-55',
            )}
          >
            {Array.from({ length: it.depth }, (_, l) => (
              <span key={l} aria-hidden className="pointer-events-none absolute inset-y-0 w-px bg-border" style={{ left: PAD + l * INDENT + 7 }} />
            ))}
            <span className="flex size-4 shrink-0 items-center justify-center text-faint">
              {it.dir ? <ChevronRight aria-hidden className={cx('size-3.5 transition-transform', it.open && 'rotate-90')} /> : it.icon}
            </span>
            <span className="min-w-0 flex-1 truncate">{it.name}</span>
            {it.trailing}
            {statusColumn && <span className="flex w-3.5 shrink-0 justify-center">{it.status}</span>}
          </button>
        );
      })}
    </div>
  );
}

const STATUS_TONE: Record<string, string> = {
  M: 'text-need',
  A: 'text-add',
  '?': 'text-add',
  D: 'text-del',
  R: 'text-info',
  U: 'text-del',
};

/** A small framed glyph for a file's status: • modified, + added/untracked, − deleted, R, !. */
export function StatusSquare({ letter }: { letter: string }) {
  const tone = STATUS_TONE[letter] ?? 'text-muted';
  const glyph =
    letter === 'M' ? (
      <span className="size-[3px] rounded-full bg-current" />
    ) : letter === 'A' || letter === '?' ? (
      <svg viewBox="0 0 8 8" className="size-2" aria-hidden>
        <path d="M4 1v6M1 4h6" stroke="currentColor" strokeWidth="1.3" />
      </svg>
    ) : letter === 'D' ? (
      <svg viewBox="0 0 8 8" className="size-2" aria-hidden>
        <path d="M1 4h6" stroke="currentColor" strokeWidth="1.3" />
      </svg>
    ) : (
      <span className="font-mono text-[8px] font-bold leading-none">{letter === 'U' ? '!' : letter}</span>
    );
  return (
    <span role="img" aria-label={t.panel.status[letter] ?? letter} title={t.panel.status[letter] ?? letter} className={cx('inline-flex size-3 shrink-0 items-center justify-center rounded-[3px] border border-current/70', tone)}>
      {glyph}
    </span>
  );
}
