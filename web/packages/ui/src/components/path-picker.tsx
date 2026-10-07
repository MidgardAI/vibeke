// A host path field with a live folder list under it (fs.browse): type to filter, Tab or → to
// complete, ↑/↓ to choose, Enter / → / click to go into a folder, ".." for the parent. Git
// repositories are marked. With `pickNative` (the desktop app on the host itself) a "Browse…"
// button opens the OS folder picker instead.

import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { CornerLeftUp, Folder, FolderGit2, FolderOpen } from 'lucide-react';
import type { BrowseResult } from '@vibeke/core';
import { t } from '../i18n';
import { browseArgs, complete, descend, filterEntries, parentOf, splitPath } from '../lib/path-picker';
import { Button, Row, Spinner, cx } from './ui';

export interface PathPickerProps {
  value: string;
  onChange(value: string): void;
  /** `fs.browse` on the host the path belongs to. */
  browse(path: string, prefix?: string): Promise<BrowseResult>;
  /** The OS folder picker (only when the host is this machine); resolves null when canceled. */
  pickNative?(defaultPath: string): Promise<string | null>;
  /** Enter with no folder chosen in the list. */
  onSubmit?(value: string): void;
  placeholder?: string;
  label?: string;
  autoFocus?: boolean;
}

/** Rows rendered at most (the host already caps a listing at 500). */
const MAX_ROWS = 200;
const DEBOUNCE_MS = 120;

type Listing = { key: string; result: BrowseResult | null; error: string | null };

export function PathPicker({ value, onChange, browse, pickNative, onSubmit, placeholder, label, autoFocus }: PathPickerProps) {
  const id = useId();
  const listId = `${id}-list`;
  const args = browseArgs(value);
  const key = `${args.path}\0${args.prefix ?? ''}`;
  const [listing, setListing] = useState<Listing | null>(null);
  const [sel, setSel] = useState(-1);
  const [picking, setPicking] = useState(false);
  const browseRef = useRef(browse);
  browseRef.current = browse;

  // Re-list only when the folder changes (typing a name filters locally).
  useEffect(() => {
    if (listing?.key === key) return;
    let live = true;
    const timer = setTimeout(() => {
      browseRef
        .current(args.path, args.prefix)
        .then((result) => live && setListing({ key, result, error: null }))
        .catch((e: unknown) => live && setListing({ key, result: null, error: e instanceof Error ? e.message : String(e) }));
    }, DEBOUNCE_MS);
    return () => {
      live = false;
      clearTimeout(timer);
    };
    // `key` covers the request arguments.
  }, [key]);

  const { prefix } = splitPath(value);
  const current = listing?.key === key ? listing : null;
  const entries = useMemo(() => filterEntries(current?.result?.entries ?? [], prefix), [current, prefix]);
  const shown = entries.slice(0, MAX_ROWS);
  const parent = current?.result?.parent ?? null;

  useEffect(() => setSel(-1), [key, prefix]);
  useEffect(() => {
    if (sel >= 0) document.getElementById(`${id}-opt-${sel}`)?.scrollIntoView({ block: 'nearest' });
  }, [id, sel]);

  const go = (next: string) => {
    onChange(next);
    setSel(-1);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    const atEnd = e.currentTarget.selectionStart === value.length && e.currentTarget.selectionEnd === value.length;
    switch (e.key) {
      case 'Tab': {
        if (e.shiftKey || e.altKey || e.metaKey || e.ctrlKey) return;
        const next = complete(value, current?.result?.entries ?? []);
        if (next !== null) {
          e.preventDefault();
          go(next);
        } else if (shown.length > 0) {
          // No progress: a second Tab walks the list.
          e.preventDefault();
          setSel((s) => (s + 1) % shown.length);
        }
        return;
      }
      case 'ArrowRight': {
        if (!atEnd) return;
        const chosen = sel >= 0 ? shown[sel] : undefined;
        if (chosen) {
          e.preventDefault();
          go(descend(value, chosen.name));
          return;
        }
        const next = complete(value, current?.result?.entries ?? []);
        if (next !== null) {
          e.preventDefault();
          go(next);
        }
        return;
      }
      case 'ArrowDown':
        if (shown.length === 0) return;
        e.preventDefault();
        setSel((s) => Math.min(s + 1, shown.length - 1));
        return;
      case 'ArrowUp':
        if (shown.length === 0) return;
        e.preventDefault();
        setSel((s) => Math.max(s - 1, -1));
        return;
      case 'Escape':
        if (sel >= 0) {
          e.preventDefault();
          e.stopPropagation();
          setSel(-1);
        }
        return;
      case 'Enter': {
        const chosen = sel >= 0 ? shown[sel] : undefined;
        if (chosen) {
          e.preventDefault();
          go(descend(value, chosen.name));
        } else if (onSubmit) {
          e.preventDefault();
          onSubmit(value);
        }
        return;
      }
    }
  };

  const native = async () => {
    if (!pickNative) return;
    const start = current?.result?.path ?? (value.startsWith('/') ? value : '');
    setPicking(true);
    try {
      const picked = await pickNative(start);
      if (picked) go(picked.endsWith('/') ? picked : `${picked}/`);
    } finally {
      setPicking(false);
    }
  };

  return (
    <div className="space-y-1.5">
      {label && (
        <label htmlFor={id} className="block text-xs text-muted">
          {label}
        </label>
      )}
      <div className="flex items-center gap-2">
        <input
          id={id}
          role="combobox"
          aria-expanded={shown.length > 0}
          aria-controls={listId}
          aria-autocomplete="list"
          aria-activedescendant={sel >= 0 ? `${id}-opt-${sel}` : undefined}
          value={value}
          placeholder={placeholder ?? t.pathPicker.placeholder}
          autoFocus={autoFocus}
          autoCapitalize="off"
          autoCorrect="off"
          autoComplete="off"
          spellCheck={false}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={onKeyDown}
          className={cx(
            'h-8 w-full min-w-0 flex-1 rounded-md border border-border bg-bg px-2.5 font-mono text-sm text-fg placeholder:text-faint pointer-coarse:h-10 pointer-coarse:text-base',
            'focus:border-border-strong focus:outline-none focus-visible:ring-2 focus-visible:ring-fg/15',
          )}
        />
        {pickNative && (
          <Button size="md" icon={<FolderOpen aria-hidden />} busy={picking} onClick={() => void native()}>
            {t.pathPicker.browse}
          </Button>
        )}
      </div>
      <div id={listId} role="listbox" aria-label={label ?? t.pathPicker.folders} className="vk-scroll max-h-60 overflow-y-auto rounded-md border border-border bg-surface p-1">
        {parent !== null && (
          <Row compact leading={<CornerLeftUp aria-hidden className="size-3.5" />} onMouseDown={(e) => e.preventDefault()} onClick={() => go(parentOf(value))} title={t.pathPicker.parent}>
            ..
          </Row>
        )}
        {!current && (
          <div className="flex h-7 items-center gap-2 px-2 text-xs text-muted">
            <Spinner /> {t.loading}
          </div>
        )}
        {current?.error && <div className="px-2 py-1.5 text-xs text-danger">{current.error}</div>}
        {current?.result && shown.length === 0 && <div className="px-2 py-1.5 text-xs text-muted">{prefix ? t.pathPicker.noMatch : t.pathPicker.empty}</div>}
        {shown.map((e, i) => (
          <Row
            key={e.name}
            id={`${id}-opt-${i}`}
            role="option"
            aria-selected={i === sel}
            compact
            active={i === sel}
            leading={e.git_repo ? <FolderGit2 aria-hidden className="size-3.5" /> : <Folder aria-hidden className="size-3.5" />}
            trailing={e.git_repo ? <span className="rounded border border-border px-1 font-mono text-[10px] leading-4 text-muted">{t.pathPicker.git}</span> : undefined}
            onMouseDown={(ev) => ev.preventDefault()}
            onClick={() => go(descend(value, e.name))}
          >
            <span className="font-mono">{e.name}</span>
          </Row>
        ))}
        {(current?.result?.truncated || entries.length > shown.length) && <div className="px-2 py-1 text-xs text-faint">{t.pathPicker.truncated}</div>}
      </div>
    </div>
  );
}
